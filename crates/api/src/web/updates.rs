//! Owner-only independent release discovery and on-demand application.
use super::state::ApiState;
use super::{session_owner, trusted_web_origin};
use anyhow::Context as _;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use jmodguard::db::{self, ComponentObservation, ComponentRelease, Db};
use jmodguard_updater::http_client_builder;
use jmodguard_updater::oci::{Registry, sha256_hex};
use jmodguard_updater::storage::{ReleaseVerification, Storage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;
use tower_sessions::Session;

fn error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message})))
}

fn check_failure(err: &anyhow::Error) -> (StatusCode, Json<Value>) {
    let unavailable = err
        .chain()
        .any(|cause| cause.to_string() == jmodguard::control::CONTROL_LINK_UNAVAILABLE);
    if unavailable {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            jmodguard::control::CONTROL_LINK_UNAVAILABLE,
        )
    } else {
        error(StatusCode::SERVICE_UNAVAILABLE, "release check failed")
    }
}

pub fn release_check_is_stale(checked_at: Option<i64>, now: i64) -> bool {
    db::release_check_is_stale(checked_at, now)
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

macro_rules! via_bot {
    ($app:expr, $direct:expr, $op:expr) => {{
        if $app.db.is_read_only() {
            commit_typed($app, $op).await
        } else {
            $direct.await
        }
    }};
}

async fn record_running(
    app: &ApiState,
    component: &str,
    release: Option<ComponentRelease>,
    observation: ComponentObservation,
    now: i64,
) -> anyhow::Result<()> {
    if app.db.is_read_only() {
        commit_typed(
            app,
            db::ControlOp::RecordComponentRunning {
                component: component.to_owned(),
                release,
                observation,
                now,
            },
        )
        .await
    } else {
        db::record_component_running(&app.db, component, release, observation, now).await
    }
}

async fn record_verified(
    app: &ApiState,
    release: &ComponentRelease,
    previous: Option<&ComponentObservation>,
    root_hash: String,
    admin_hash: String,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let observation = ComponentObservation {
        guild_schema: None,
        update_protocol: 0,
        launcher_revision: 0,
        baked_launcher_revision: 0,
        source_sha: release.source_sha.clone(),
        executable_sha256: root_hash,
        admin_sha256: Some(admin_hash),
        base_image_sha: "Storage Zone".into(),
        started_at: previous.map_or(now, |seen| seen.started_at),
        min_schema: release.min_schema,
        max_schema: release.max_schema,
        api_capability: 1,
        self_update_enabled: true,
        base_abi: "static-webui-v1".into(),
    };
    if app.db.is_read_only() {
        commit_typed(
            app,
            db::ControlOp::RecordWebuiInstalledVerified {
                release: release.clone(),
                observation,
                now,
            },
        )
        .await
    } else {
        db::record_webui_installed_verified(&app.db, release, observation, now).await
    }
}

async fn commit_typed<T: serde::de::DeserializeOwned>(
    app: &ApiState,
    op: db::ControlOp,
) -> anyhow::Result<T> {
    let (_, body) = app.commit_json(op).await?;
    Ok(serde_json::from_value(body)?)
}

async fn claim_any(
    app: Option<&ApiState>,
    db: &Db,
    component: &str,
    owner: &str,
    now: i64,
    ttl: i64,
) -> anyhow::Result<db::UpdateLeaseStatus> {
    if db.is_read_only() {
        commit_typed(
            app.context("waiting")?,
            db::ControlOp::ClaimUpdateLease {
                component: component.to_owned(),
                owner: owner.to_owned(),
                now,
                ttl,
            },
        )
        .await
    } else {
        db::claim_update_lease(db, component, owner, now, ttl)
            .await
            .map(Into::into)
    }
}

async fn renew_any(
    app: Option<&ApiState>,
    db: &Db,
    component: &str,
    owner: &str,
    now: i64,
    ttl: i64,
) -> anyhow::Result<db::UpdateLeaseStatus> {
    if db.is_read_only() {
        commit_typed(
            app.context("waiting")?,
            db::ControlOp::RenewUpdateLease {
                component: component.to_owned(),
                owner: owner.to_owned(),
                now,
                ttl,
            },
        )
        .await
    } else {
        db::renew_update_lease(db, component, owner, now, ttl)
            .await
            .map(Into::into)
    }
}

async fn release_any(
    app: Option<&ApiState>,
    db: &Db,
    component: &str,
    owner: &str,
) -> anyhow::Result<()> {
    if db.is_read_only() {
        commit_typed(
            app.context("waiting")?,
            db::ControlOp::ReleaseUpdateLease {
                component: component.to_owned(),
                owner: owner.to_owned(),
            },
        )
        .await
    } else {
        db::release_update_lease(db, component, owner).await
    }
}

async fn owner(app: &Arc<ApiState>, session: &Session) -> Result<(), (StatusCode, Json<Value>)> {
    match session_owner(app, session).await {
        Ok(_) => Ok(()),
        Err(failure) if failure.0.is::<super::SessionStoreUnavailable>() => Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session store unavailable",
        )),
        Err(failure) if failure.0.is::<super::AuthLost>() => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"discord authorization failed", "code":"auth_lost"})),
        )),
        Err(failure) if failure.0.is::<super::NotAuthenticated>() => {
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

/// The component this request just wrote replaces the replica row. A plain
/// read can still be behind that commit.
pub(crate) fn overlay_written_component(
    mut rows: Vec<db::ComponentUpdate>,
    written: &db::ComponentUpdate,
) -> Vec<db::ComponentUpdate> {
    match rows
        .iter()
        .position(|row| row.component == written.component)
    {
        Some(index) => rows[index] = written.clone(),
        None => rows.push(written.clone()),
    }
    rows
}

pub async fn status(State(app): State<Arc<ApiState>>, session: Session) -> Reply {
    owner(&app, &session).await?;
    status_response(&app, None).await
}

async fn status_response(app: &Arc<ApiState>, written: Option<db::ComponentUpdate>) -> Reply {
    let mut updates = db::component_updates(&app.db)
        .await
        .map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "update state unavailable"))?;
    if let Some(written) = written.as_ref() {
        updates = overlay_written_component(updates, written);
    }
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
        "boot_mismatches":db::read_boot_mismatch_map(&app.db).await.map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "boot mismatch state unavailable"))?,
        "webui_purge_pending":db::webui_purge_pending(&app.db).await.map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "WebUI purge state unavailable"))?,
        "alert_channel":db::update_alert_channel(&app.db).await.map_err(|_| error(StatusCode::SERVICE_UNAVAILABLE, "alert channel unavailable"))?.map(|id| id.to_string()),
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

const BUNNY_API: &str = "https://api.bunny.net";
const PURGE_ATTEMPTS: u32 = 6;
const PURGE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Pull Zone purge target for a managed WebUI install. The key is a scoped
/// PullZone.Purge API key, never the account API key.
struct PullZonePurge {
    zone: String,
    key: String,
}

impl PullZonePurge {
    /// Both values unset skips the purge; exactly one set is partial
    /// configuration and fails closed before Storage is touched.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<Self>> {
        let read = |name: &str| lookup(name).filter(|value| !value.is_empty());
        match (read("BUNNY_PULL_ZONE_ID"), read("BUNNY_PURGE_API_KEY")) {
            (None, None) => Ok(None),
            (Some(zone), Some(key)) => {
                anyhow::ensure!(
                    !zone.starts_with('0') && zone.bytes().all(|c| c.is_ascii_digit()),
                    "BUNNY_PULL_ZONE_ID must be a positive numeric Pull Zone ID"
                );
                anyhow::ensure!(
                    axum::http::HeaderValue::from_str(&key).is_ok(),
                    "BUNNY_PURGE_API_KEY is not a valid header value"
                );
                Ok(Some(Self { zone, key }))
            }
            _ => anyhow::bail!(
                "set both BUNNY_PULL_ZONE_ID and BUNNY_PURGE_API_KEY to purge the Pull Zone, or neither to skip the purge"
            ),
        }
    }
}

/// Purge the WebUI Pull Zone after a successful Storage publish. Returns
/// whether a purge was sent and accepted. Never logs the key or a response body.
async fn purge_webui_cache(
    purge: Option<&PullZonePurge>,
    api: &str,
    retry_delay: Duration,
) -> anyhow::Result<bool> {
    let Some(purge) = purge else {
        tracing::warn!(
            "BUNNY_PULL_ZONE_ID and BUNNY_PURGE_API_KEY are unset; skipping Pull Zone purge, so cached WebUI HTML may stay stale until its cache time expires"
        );
        return Ok(false);
    };
    let http = http_client_builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = format!("{api}/pullzone/{}/purgeCache", purge.zone);
    for attempt in 1..=PURGE_ATTEMPTS {
        match http
            .post(&url)
            .header("AccessKey", &purge.key)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
        {
            Ok(response)
                if matches!(
                    response.status(),
                    reqwest::StatusCode::OK | reqwest::StatusCode::NO_CONTENT
                ) =>
            {
                return Ok(true);
            }
            Ok(response) => tracing::warn!(
                attempt,
                status = response.status().as_u16(),
                "Bunny Pull Zone purge attempt failed"
            ),
            Err(error) => tracing::warn!(
                attempt,
                "Bunny Pull Zone purge attempt failed: {}",
                error.without_url()
            ),
        }
        if attempt < PURGE_ATTEMPTS {
            tokio::time::sleep(retry_delay).await;
        }
    }
    anyhow::bail!(
        "Bunny Pull Zone purge failed after {PURGE_ATTEMPTS} attempts; Storage already has the new WebUI files, and the CDN may serve stale HTML until a purge succeeds"
    )
}

async fn set_purge_pending(app: &ApiState, error: Option<String>) -> anyhow::Result<()> {
    via_bot!(
        app,
        db::set_webui_purge_pending(&app.db, error.clone()),
        db::ControlOp::SetWebuiPurgePending { error }
    )
}

/// Purge and record the outcome. A failure leaves a durable pending-purge
/// marker that Storage reconciliation does not clear; only a later accepted
/// purge clears it. Published Storage files are never rolled back for it.
async fn purge_and_record(
    app: &ApiState,
    purge: Option<&PullZonePurge>,
    api: &str,
    retry_delay: Duration,
) -> anyhow::Result<()> {
    match purge_webui_cache(purge, api, retry_delay).await {
        Ok(false) => Ok(()),
        Ok(true) => {
            // Read first so a purge with nothing pending sends no extra write.
            let cleared = match db::webui_purge_pending(&app.db).await {
                Ok(None) => Ok(()),
                Ok(Some(_)) => set_purge_pending(app, None).await,
                Err(error) => Err(error),
            };
            if let Err(error) = cleared {
                tracing::warn!("WebUI pending purge marker was not cleared: {error:#}");
            }
            Ok(())
        }
        Err(error) => {
            if let Err(record_error) = set_purge_pending(app, Some(format!("{error:#}"))).await {
                tracing::warn!("WebUI pending purge could not be recorded: {record_error:#}");
            }
            Err(error)
        }
    }
}

/// Pending-purge marker held while a configured install publishes and purges.
const PURGE_PENDING_DURING_INSTALL: &str =
    "WebUI Storage publication started before a Pull Zone purge was accepted";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationPurgeMarker {
    Unchanged,
    Created,
}

/// Managed WebUI install order: purge configuration is checked and, when
/// configured, the pending-purge marker is made durable before `publish` runs,
/// so a restart or failed purge after the Storage cutover stays retryable. The
/// install fails closed if the marker cannot be kept. The Pull Zone is purged
/// only after publication succeeds; a publication failure sends no purge and
/// restores the marker's previous state.
async fn publish_then_purge<T>(
    app: &ApiState,
    lookup: impl Fn(&str) -> Option<String>,
    publish: impl AsyncFnOnce() -> anyhow::Result<T>,
    api: &str,
    retry_delay: Duration,
) -> anyhow::Result<T> {
    let purge = PullZonePurge::from_lookup(lookup)?;
    let mut marker = PublicationPurgeMarker::Unchanged;
    if purge.is_some() && db::webui_purge_pending(&app.db).await?.is_none() {
        set_purge_pending(app, Some(PURGE_PENDING_DURING_INSTALL.to_owned()))
            .await
            .context("WebUI pending purge could not be recorded before publication")?;
        marker = PublicationPurgeMarker::Created;
    }
    let published = match publish().await {
        Ok(published) => published,
        Err(error) => {
            if marker == PublicationPurgeMarker::Created
                && let Err(clear_error) = set_purge_pending(app, None).await
            {
                tracing::warn!("WebUI pending purge marker was not cleared: {clear_error:#}");
            }
            return Err(error);
        }
    };
    purge_and_record(app, purge.as_ref(), api, retry_delay).await?;
    Ok(published)
}

/// A WebUI check retries an unresolved purge when both purge values are set.
async fn retry_pending_purge(
    app: &ApiState,
    lookup: impl Fn(&str) -> Option<String>,
    api: &str,
    retry_delay: Duration,
) -> anyhow::Result<()> {
    if db::webui_purge_pending(&app.db).await?.is_none() {
        return Ok(());
    }
    match PullZonePurge::from_lookup(lookup)? {
        Some(purge) => purge_and_record(app, Some(&purge), api, retry_delay).await,
        None => Ok(()),
    }
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
    let written = check_one(&app, &input.component)
        .await
        .map_err(|error| check_failure(&error))?;
    status_response(&app, Some(written)).await
}

pub async fn check_one(
    app: &Arc<ApiState>,
    component: &str,
) -> anyhow::Result<db::ComponentUpdate> {
    check_one_with(
        app,
        component,
        async |component, channel| Registry::ghcr()?.discover(component, channel).await,
        async |adopt_desired| reconcile_webui(app, adopt_desired).await,
    )
    .await
}

async fn check_one_with(
    app: &Arc<ApiState>,
    component: &str,
    discover: impl AsyncFnOnce(&str, &str) -> anyhow::Result<ComponentRelease>,
    reconcile: impl AsyncFnOnce(bool) -> anyhow::Result<()>,
) -> anyhow::Result<db::ComponentUpdate> {
    let lease = format!("{:032x}", rand::random::<u128>());
    let now = chrono::Utc::now().timestamp();
    if claim_any(Some(app), &app.db, component, &lease, now, 240).await?
        == db::UpdateLeaseStatus::Unavailable
    {
        anyhow::bail!("another update operation is running");
    }
    with_update_lease(Some(app), &app.db, component, &lease, 240, async {
        let state = db::component_update(&app.db, component).await?;
        let checked = match discover(component, &state.channel).await {
            Ok(candidate) => {
                record_release_check(app, component, &state.channel, Some(candidate), None, now)
                    .await?
            }
            Err(error) => {
                record_release_check(
                    app,
                    component,
                    &state.channel,
                    None,
                    Some(format!("{error:#}")),
                    now,
                )
                .await?;
                // Channel discovery authorizes new candidates. A failed
                // lookup still permits a signed readback of the installed digest.
                if component == "webui"
                    && let Err(error) = reconcile(false).await
                {
                    tracing::warn!("WebUI Storage reconciliation failed: {error:#}");
                }
                return Err(error);
            }
        };
        if component == "webui" {
            match reconcile(true).await {
                Ok(()) => {
                    if let Some(desired) = checked.desired.clone() {
                        via_bot!(
                            app,
                            db::settle_interrupted_webui(&app.db, &desired),
                            db::ControlOp::SettleInterruptedWebui {
                                release: desired.clone(),
                            }
                        )?;
                    }
                }
                Err(error) => tracing::warn!("WebUI Storage reconciliation failed: {error:#}"),
            }
            if let Err(error) = retry_pending_purge(
                app,
                |name| std::env::var(name).ok(),
                BUNNY_API,
                PURGE_RETRY_DELAY,
            )
            .await
            {
                tracing::warn!("WebUI pending Pull Zone purge is still unresolved: {error:#}");
            }
            if let Ok(storage) = storage()
                && let Err(error) = cleanup_webui_assets(app, &storage).await
            {
                tracing::warn!("WebUI managed asset cleanup failed: {error:#}");
            }
        }
        Ok(checked)
    })
    .await
}

async fn with_update_lease<T, F>(
    app: Option<&ApiState>,
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
        renew_any(
            app,
            db,
            component,
            lease,
            chrono::Utc::now().timestamp(),
            ttl
        )
        .await?
            == db::UpdateLeaseStatus::Held,
        "update lease expired before work began"
    );
    let watchdog = async {
        loop {
            tokio::time::sleep(Duration::from_secs((ttl / 4).max(1) as u64)).await;
            let now = chrono::Utc::now().timestamp();
            anyhow::ensure!(
                renew_any(app, db, component, lease, now, ttl).await?
                    == db::UpdateLeaseStatus::Held,
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
    let owned = renew_any(app, db, component, lease, now, ttl).await?;
    release_any(app, db, component, lease).await?;
    anyhow::ensure!(
        owned == db::UpdateLeaseStatus::Held,
        "update lease expired or changed"
    );
    result
}

async fn reconcile_webui(app: &Arc<ApiState>, adopt_desired: bool) -> anyhow::Result<()> {
    let storage = &storage()?;
    reconcile_webui_with(
        app,
        adopt_desired,
        |path| storage.get(path),
        |release| async move { storage.verify_release(&Registry::ghcr()?, &release).await },
        |release| async move {
            storage
                .verify_installed_release(
                    &Registry::ghcr()?,
                    &db::UpdateControl::from_application(&app.db),
                    &release,
                )
                .await
        },
    )
    .await
}

async fn reconcile_webui_with<Read, Desired, Installed>(
    app: &ApiState,
    adopt_desired: bool,
    read: impl Fn(&'static str) -> Read,
    verify_desired: impl Fn(ComponentRelease) -> Desired,
    verify_installed: impl Fn(ComponentRelease) -> Installed,
) -> anyhow::Result<()>
where
    Read: Future<Output = anyhow::Result<Option<Vec<u8>>>> + Send,
    Desired: Future<Output = anyhow::Result<ReleaseVerification>> + Send,
    Installed: Future<Output = anyhow::Result<ReleaseVerification>> + Send,
{
    let state = db::component_update(&app.db, "webui").await?;
    let html = if adopt_desired {
        Some((
            read("index.html").await?,
            read("administration/index.html").await?,
        ))
    } else {
        None
    };
    // Only a switch to a different digest can prove that a desired release
    // was activated. Channel aliases can name the same installed image.
    if let Some((root, admin)) = html
        && let Some(release) = state.desired.as_ref().filter(|desired| {
            state
                .installed
                .as_ref()
                .is_none_or(|installed| desired.digest != installed.digest)
        })
    {
        let prefix = format!("/assets/releases/{}/", release.source_sha);
        if root.as_ref().is_some_and(|bytes| {
            bytes
                .windows(prefix.len())
                .any(|part| part == prefix.as_bytes())
        }) && admin.as_ref().is_some_and(|bytes| {
            bytes
                .windows(prefix.len())
                .any(|part| part == prefix.as_bytes())
        }) && let ReleaseVerification::Verified {
            root_hash,
            admin_hash,
        } = verify_desired(release.clone()).await?
        {
            record_running(
                app,
                "webui",
                Some(release.clone()),
                ComponentObservation {
                    guild_schema: None,
                    update_protocol: 0,
                    launcher_revision: 0,
                    baked_launcher_revision: 0,
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
    if let Some(installed) = state.installed.as_ref()
        && let ReleaseVerification::Verified {
            root_hash,
            admin_hash,
        } = verify_installed(installed.clone()).await?
    {
        return record_verified(
            app,
            installed,
            state.observed.as_ref(),
            root_hash,
            admin_hash,
        )
        .await;
    }
    if state.observed.is_some() {
        via_bot!(
            app,
            db::record_webui_drift(&app.db, "Storage files differ from the installed release"),
            db::ControlOp::RecordWebuiDrift {
                error: "Storage files differ from the installed release".to_owned(),
            }
        )?;
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
            via_bot!(
                app,
                db::untrack_webui_assets(&app.db, &sha),
                db::ControlOp::UntrackWebuiAssets { sha: sha.clone() }
            )?;
        }
    }
    Ok(())
}

pub const RELEASE_CHECK_INTERVAL: Duration = Duration::from_secs(3600);
pub const RELEASE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Drain the background release-check task after HTTP graceful shutdown.
pub const RELEASE_CHECK_SHUTDOWN_DRAIN: Duration = Duration::from_secs(60);

fn control_link_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string() == jmodguard::control::CONTROL_LINK_UNAVAILABLE)
}

fn warn_record_release_check(component: &str, error: &anyhow::Error) {
    if control_link_unavailable(error) {
        tracing::warn!(component, "control link unavailable during release check");
    } else {
        tracing::warn!(component, "failed to record release check: {error:#}");
    }
}

/// Outcome of one [`record_release_checks`] pass.
pub struct ReleaseCheckBatch {
    pub finished: bool,
    /// Components whose update lease was held; skip automatic install this tick.
    pub lease_held: HashSet<&'static str>,
}

async fn record_release_check(
    app: &Arc<ApiState>,
    component: &str,
    channel: &str,
    candidate: Option<ComponentRelease>,
    error: Option<String>,
    now: i64,
) -> anyhow::Result<db::ComponentUpdate> {
    via_bot!(
        app,
        db::record_update_check(
            &app.db,
            component,
            channel,
            candidate.clone(),
            error.clone(),
            now
        ),
        db::ControlOp::RecordUpdateCheck {
            component: component.to_owned(),
            channel: channel.to_owned(),
            candidate,
            error,
            now,
        }
    )
}

/// Record configured-channel digest lookups through the bot writer. Discovery is
/// injected so tests do not call the registry.
pub async fn record_release_checks<F>(
    app: &Arc<ApiState>,
    discover: F,
    lookup_timeout: Duration,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
) -> ReleaseCheckBatch
where
    F: FnMut(&str, &str) -> Pin<Box<dyn Future<Output = anyhow::Result<ComponentRelease>> + Send>>,
{
    record_release_checks_with(
        app,
        discover,
        lookup_timeout,
        shutdown,
        async |database, installed| {
            storage()?
                .verify_installed_release(
                    &Registry::ghcr()?,
                    &db::UpdateControl::from_application(database),
                    installed,
                )
                .await
        },
    )
    .await
}

async fn record_release_checks_with<F>(
    app: &Arc<ApiState>,
    mut discover: F,
    lookup_timeout: Duration,
    mut shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    verify_webui: impl AsyncFn(&Db, &ComponentRelease) -> anyhow::Result<ReleaseVerification>,
) -> ReleaseCheckBatch
where
    F: FnMut(&str, &str) -> Pin<Box<dyn Future<Output = anyhow::Result<ComponentRelease>> + Send>>,
{
    let mut lease_held = HashSet::new();
    for component in ["bot", "api", "webui"] {
        let state = match db::component_update(&app.db, component).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(component, "release read failed: {error:#}");
                continue;
            }
        };
        let channel = state.channel.clone();
        let lease = format!("{:032x}", rand::random::<u128>());
        let claim_now = chrono::Utc::now().timestamp();
        let claimed = match claim_any(Some(app), &app.db, component, &lease, claim_now, 240).await {
            Ok(value) => value,
            Err(error) => {
                if control_link_unavailable(&error) || error.root_cause().to_string() == "waiting" {
                    tracing::info!(component, "release check skipped: {error:#}");
                } else {
                    tracing::warn!(component, "release check skipped: {error:#}");
                }
                continue;
            }
        };
        if claimed == db::UpdateLeaseStatus::Unavailable {
            tracing::debug!(component, "release check skipped: update lease held");
            lease_held.insert(component);
            continue;
        }
        let work = with_update_lease(Some(app), &app.db, component, &lease, 240, async {
            let outcome = tokio::select! {
                biased;
                _ = &mut shutdown => anyhow::bail!("release check shutdown"),
                outcome = timeout(lookup_timeout, discover(component, &channel)) => outcome,
            };
            let now = chrono::Utc::now().timestamp();
            let record = match outcome {
                Err(_) => Err("release lookup timed out".into()),
                Ok(Err(error)) => Err(format!("{error:#}")),
                Ok(Ok(release)) => {
                    if let Err(error) = release.validate() {
                        Err(format!("{error:#}"))
                    } else if release.component != component || release.channel != channel {
                        Err("candidate component or channel mismatch".into())
                    } else {
                        Ok(release)
                    }
                }
            };
            let (candidate, error) = match record {
                Ok(release) => (Some(release), None),
                Err(message) => (None, Some(message)),
            };
            let current = record_release_check(app, component, &channel, candidate, error, now)
                .await
                .inspect_err(|error| warn_record_release_check(component, error))?;
            if component == "webui" {
                // Refresh the installed bytes even if the moving tag lookup
                // failed. Keep readback under the same lease as publication.
                if let Some(installed) = current.installed.as_ref() {
                    let refresh = tokio::select! {
                        biased;
                        _ = &mut shutdown => anyhow::bail!("release check shutdown"),
                        result = timeout(RELEASE_LOOKUP_TIMEOUT, verify_webui(&app.db, installed)) => result,
                    };
                    let observation_result = match refresh {
                        Ok(Ok(ReleaseVerification::Verified {
                            root_hash,
                            admin_hash,
                        })) => record_verified(
                            app,
                            installed,
                            current.observed.as_ref(),
                            root_hash,
                            admin_hash,
                        )
                        .await
                        .context("recording verified WebUI observation"),
                        Ok(Ok(ReleaseVerification::Mismatch)) => {
                            // A definite mismatch invalidates the verified
                            // observation, as manual reconciliation does. A
                            // queued or failed request keeps its state and error
                            // for the manual check that adopts or settles it.
                            tracing::warn!("WebUI observation refresh found changed Storage files");
                            if current.observed.is_some() {
                                via_bot!(
                                    app,
                                    db::record_webui_drift(
                                        &app.db,
                                        "Storage files differ from the installed release"
                                    ),
                                    db::ControlOp::RecordHourlyWebuiDrift {
                                        error: "Storage files differ from the installed release"
                                            .to_owned(),
                                    }
                                )
                                .context("recording WebUI Storage drift")
                            } else {
                                Ok(())
                            }
                        }
                        Ok(Err(error)) => Err(error),
                        Err(_) => {
                            tracing::warn!("WebUI observation refresh timed out");
                            Ok(())
                        }
                    };
                    // Discovery is already durable. Observation failures
                    // cannot disqualify an otherwise eligible install.
                    if let Err(error) = observation_result {
                        tracing::warn!("WebUI observation refresh failed: {error:#}");
                    }
                }
            }
            Ok(())
        });
        match work.await {
            Ok(()) => {}
            Err(error) if error.to_string() == "release check shutdown" => {
                return ReleaseCheckBatch {
                    finished: false,
                    lease_held,
                };
            }
            Err(error) => {
                tracing::warn!(component, "background release check failed: {error:#}");
                lease_held.insert(component);
            }
        }
    }
    ReleaseCheckBatch {
        finished: true,
        lease_held,
    }
}

/// Promoted channels may install on a cadence. `latest` cannot, and cadence
/// `off` never does. This is the same pair `set_update_policy` allows.
pub fn scheduled_auto_install_allowed(channel: &str, cadence: &str) -> bool {
    matches!(channel, "stable" | "nightly") && matches!(cadence, "daily" | "weekly" | "monthly")
}

/// Unattended apply gates for [`poll_scheduled_installs`] and
/// [`run_release_check_loop`]. Production passes [`ScheduledInstallPolicy::from_env`];
/// integration tests inject explicit values instead of mutating process env.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledInstallPolicy {
    pub unattended_updates: bool,
    pub apply_enabled: bool,
}

impl ScheduledInstallPolicy {
    pub fn from_env() -> Self {
        Self {
            unattended_updates: enabled("JMODGUARD_UNATTENDED_UPDATES"),
            apply_enabled: enabled("JMODGUARD_UPDATE_APPLY_ENABLED"),
        }
    }

    pub fn enabled(unattended_updates: bool, apply_enabled: bool) -> Self {
        Self {
            unattended_updates,
            apply_enabled,
        }
    }

    pub fn allows_scheduled_install(&self) -> bool {
        self.unattended_updates && self.apply_enabled
    }
}

/// The install half of [`poll_all`]. Flags still default off. Rows that are
/// not a stable or nightly cadence are left untouched.
pub async fn poll_scheduled_installs(
    app: &Arc<ApiState>,
    skip_lease_held: &HashSet<&'static str>,
    policy: ScheduledInstallPolicy,
) {
    if !policy.allows_scheduled_install() {
        return;
    }
    for component in ["bot", "api", "webui"] {
        if skip_lease_held.contains(component) {
            continue;
        }
        let state = match db::component_update(&app.db, component).await {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(component, "automatic update skipped: {error:#}");
                continue;
            }
        };
        if !scheduled_auto_install_allowed(&state.channel, &state.cadence) {
            continue;
        }
        if let Err(error) = automatic_install(app, component).await {
            tracing::warn!(component, "automatic update skipped: {error:#}");
        }
    }
}

/// Coalesced or interval-based tick source for [`run_release_check_loop`].
pub struct ReleaseCheckTick {
    inner: ReleaseCheckTickInner,
}

enum ReleaseCheckTickInner {
    Interval(tokio::time::Interval),
    Coalescing {
        notify: Arc<Notify>,
        parked: Option<tokio::sync::watch::Sender<u32>>,
    },
}

impl ReleaseCheckTick {
    pub fn interval(duration: Duration) -> Self {
        let mut tick = tokio::time::interval(duration);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            inner: ReleaseCheckTickInner::Interval(tick),
        }
    }

    pub fn coalescing(notify: Arc<Notify>, parked: tokio::sync::watch::Sender<u32>) -> Self {
        Self {
            inner: ReleaseCheckTickInner::Coalescing {
                notify,
                parked: Some(parked),
            },
        }
    }

    pub fn fire(&self) {
        if let ReleaseCheckTickInner::Coalescing { notify, .. } = &self.inner {
            notify.notify_one();
        }
    }

    async fn wait_for_tick(&mut self, shutdown: Pin<Box<dyn Future<Output = ()> + Send>>) -> bool {
        match &mut self.inner {
            ReleaseCheckTickInner::Interval(tick) => {
                tokio::select! {
                    biased;
                    _ = shutdown => false,
                    _ = tick.tick() => true,
                }
            }
            ReleaseCheckTickInner::Coalescing { notify, .. } => {
                tokio::select! {
                    biased;
                    _ = shutdown => false,
                    _ = notify.notified() => true,
                }
            }
        }
    }

    fn signal_parked(&self) {
        if let ReleaseCheckTickInner::Coalescing {
            parked: Some(tx), ..
        } = &self.inner
        {
            let _ = tx.send_if_modified(|generation| {
                *generation += 1;
                true
            });
        }
    }
}

/// Background release checks for the configured channel. Each tick records the
/// digest, then runs [`poll_scheduled_installs`] for stable and nightly
/// cadences. `latest` and cadence `off` stay record-only. A tick finishes
/// before the next one starts.
pub async fn run_release_check_loop<F, S>(
    app: Arc<ApiState>,
    mut tick: ReleaseCheckTick,
    lookup_timeout: Duration,
    mut discover: F,
    shutdown: S,
    policy: ScheduledInstallPolicy,
) where
    F: FnMut(&str, &str) -> Pin<Box<dyn Future<Output = anyhow::Result<ComponentRelease>> + Send>>,
    S: Fn() -> Pin<Box<dyn Future<Output = ()> + Send>>,
{
    loop {
        if !tick.wait_for_tick(shutdown()).await {
            break;
        }
        let batch = record_release_checks(&app, &mut discover, lookup_timeout, shutdown()).await;
        if !batch.finished {
            break;
        }
        poll_scheduled_installs(&app, &batch.lease_held, policy).await;
        tick.signal_parked();
    }
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

/// Log-only registry poll plus the scheduled install. The hourly task does not
/// call this: it records through [`record_release_checks`], then
/// [`poll_scheduled_installs`], so a second unrecorded lookup does not run.
pub async fn poll_all(app: &Arc<ApiState>) {
    poll_releases(app, async |component, channel| {
        let registry = Registry::ghcr()?;
        Ok(registry.discover(component, channel).await?.digest)
    })
    .await;
    poll_scheduled_installs(app, &HashSet::new(), ScheduledInstallPolicy::from_env()).await;
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
        if claim_any(Some(app), &app.db, component, &lease, now, 600).await?
            == db::UpdateLeaseStatus::Unavailable
        {
            return Ok(());
        }
        Some(lease)
    } else {
        None
    };
    let requested = via_bot!(
        app,
        db::request_due_automatic_install(&app.db, component, &candidate.digest, now, true),
        db::ControlOp::RequestDueAutomaticInstall {
            component: component.to_owned(),
            expected_digest: candidate.digest.clone(),
            now,
            unattended_allowed: true,
        }
    );
    let release = match requested {
        Ok(Some(release)) => release,
        Ok(None) => {
            if let Some(lease) = &lease {
                release_any(Some(app), &app.db, component, lease).await?;
            }
            return Ok(());
        }
        Err(error) => {
            if let Some(lease) = &lease {
                release_any(Some(app), &app.db, component, lease).await?;
            }
            return Err(error);
        }
    };
    if component == "webui" {
        let lease = lease.expect("WebUI install owns its lease");
        if let Err(error) = run_webui_install(app, release, &lease).await {
            tracing::error!("automatic WebUI install failed: {error:#}");
        }
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
    let written = via_bot!(
        app.as_ref(),
        db::set_update_policy(
            &app.db,
            &input.component,
            &input.channel,
            &input.cadence,
            enabled("JMODGUARD_UNATTENDED_UPDATES"),
        ),
        db::ControlOp::SetUpdatePolicy {
            component: input.component.clone(),
            channel: input.channel.clone(),
            cadence: input.cadence.clone(),
            unattended_allowed: enabled("JMODGUARD_UNATTENDED_UPDATES"),
        }
    )
    .map_err(|problem| error(StatusCode::BAD_REQUEST, &format!("{problem:#}")))?;
    status_response(&app, Some(written)).await
}

#[derive(Deserialize)]
pub struct AlertChannelRequest {
    /// Decimal Discord channel ID; empty clears the boot repair alert channel.
    channel_id: String,
}

pub async fn alert_channel(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<AlertChannelRequest>,
) -> Reply {
    owner(&app, &session).await?;
    origin(&app, &headers)?;
    let trimmed = input.channel_id.trim();
    let channel = if trimmed.is_empty() {
        None
    } else {
        Some(
            trimmed
                .parse::<u64>()
                .ok()
                .filter(|id| *id != 0)
                .ok_or_else(|| {
                    error(
                        StatusCode::BAD_REQUEST,
                        "alert channel must be a Discord channel ID",
                    )
                })?,
        )
    };
    via_bot!(
        app.as_ref(),
        db::set_update_alert_channel(&app.db, channel),
        db::ControlOp::SetUpdateAlertChannel {
            channel_id: channel
        }
    )
    .map_err(|problem| error(StatusCode::BAD_REQUEST, &format!("{problem:#}")))?;
    // A replica read can trail the write just committed through the bot.
    let Json(mut body) = status_response(&app, None).await?;
    body["alert_channel"] = json!(channel.map(|id| id.to_string()));
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct InstallRequest {
    component: String,
    digest: String,
    #[serde(default)]
    launcher: bool,
}

async fn queue_webui_install(
    app: Option<&ApiState>,
    db: &Db,
    digest: &str,
    now: i64,
) -> anyhow::Result<(ComponentRelease, String)> {
    let lease = format!("{:032x}", rand::random::<u128>());
    anyhow::ensure!(
        claim_any(app, db, "webui", &lease, now, 600).await? == db::UpdateLeaseStatus::Held,
        "another WebUI operation is running"
    );
    let requested = if db.is_read_only() {
        commit_typed(
            app.context("waiting")?,
            db::ControlOp::RequestComponentInstall {
                component: "webui".to_owned(),
                digest: digest.to_owned(),
                now,
            },
        )
        .await
    } else {
        db::request_component_install(db, "webui", digest, now).await
    };
    match requested {
        Ok(release) => Ok((release, lease)),
        Err(error) => {
            release_any(app, db, "webui", &lease).await?;
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
    if release_check_is_stale(record.checked_at, now) {
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
        let (release, lease) = queue_webui_install(Some(app.as_ref()), &app.db, &input.digest, now)
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
            via_bot!(
                app.as_ref(),
                db::request_launcher_install(&app.db, &input.component, &input.digest, now),
                db::ControlOp::RequestLauncherInstall {
                    component: input.component.clone(),
                    digest: input.digest.clone(),
                    now,
                }
            )
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
            via_bot!(
                app.as_ref(),
                db::request_component_install(&app.db, &input.component, &input.digest, now),
                db::ControlOp::RequestComponentInstall {
                    component: input.component.clone(),
                    digest: input.digest.clone(),
                    now,
                }
            )
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
    with_update_lease(Some(app), &app.db, "webui", lease, 600, async {
        let result = apply_webui(app, &release).await;
        if let Err(error) = &result {
            via_bot!(
                app,
                db::record_component_failure(&app.db, "webui", &release, &format!("{error:#}")),
                db::ControlOp::RecordComponentFailure {
                    component: "webui".to_owned(),
                    failed: release.clone(),
                    error: format!("{error:#}"),
                }
            )?;
            if let Err(reconcile_error) = reconcile_webui(app, true).await {
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
        publish_then_purge(
            app,
            |name| std::env::var(name).ok(),
            async || {
                via_bot!(
                    app,
                    db::track_webui_assets(&app.db, &release.source_sha, now),
                    db::ControlOp::TrackWebuiAssets {
                        sha: release.source_sha.clone(),
                        now,
                    }
                )?;
                let registry = Registry::ghcr()?;
                storage.publish(&registry, release).await
            },
            BUNNY_API,
            PURGE_RETRY_DELAY,
        )
        .await?;
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
            baked_launcher_revision: 0,
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
        record_running(
            app,
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

    #[derive(Default)]
    struct PurgeFixture {
        statuses: Vec<StatusCode>,
        requests: Vec<(String, String, String)>,
    }

    /// Notifies when a Pull Zone purge attempt is logged as failed.
    #[derive(Clone, Default)]
    struct PurgeAttemptFailed(Arc<tokio::sync::Notify>);

    impl tracing::Subscriber for PurgeAttemptFailed {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() == "jmodguard_api::web::updates"
                && event.fields().any(|field| field.name() == "attempt")
            {
                self.0.notify_one();
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Local Bunny API stand-in: answers scripted statuses in order, then 200.
    async fn purge_server(
        statuses: Vec<StatusCode>,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<PurgeFixture>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn endpoint(
            State(state): State<Arc<tokio::sync::Mutex<PurgeFixture>>>,
            method: axum::http::Method,
            uri: axum::http::Uri,
            headers: HeaderMap,
            body: String,
        ) -> StatusCode {
            let mut fixture = state.lock().await;
            let key = headers
                .get("AccessKey")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            fixture
                .requests
                .push((format!("{method} {}", uri.path()), key, body));
            if fixture.statuses.is_empty() {
                StatusCode::OK
            } else {
                fixture.statuses.remove(0)
            }
        }
        let state = Arc::new(tokio::sync::Mutex::new(PurgeFixture {
            statuses,
            ..PurgeFixture::default()
        }));
        let router = axum::Router::new()
            .fallback(axum::routing::any(endpoint))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}"), state, server)
    }

    fn purge_env(zone: Option<&str>, key: Option<&str>) -> anyhow::Result<Option<PullZonePurge>> {
        PullZonePurge::from_lookup(|name| match name {
            "BUNNY_PULL_ZONE_ID" => zone.map(str::to_owned),
            "BUNNY_PURGE_API_KEY" => key.map(str::to_owned),
            "BUNNY_API_KEY" => Some("account-key-must-not-be-used".into()),
            _ => None,
        })
    }

    async fn run_purge(
        zone: Option<&str>,
        key: Option<&str>,
        statuses: Vec<StatusCode>,
    ) -> (anyhow::Result<bool>, Vec<(String, String, String)>) {
        let (api, state, server) = purge_server(statuses).await;
        let result = match purge_env(zone, key) {
            Ok(purge) => purge_webui_cache(purge.as_ref(), &api, Duration::from_millis(1)).await,
            Err(error) => Err(error),
        };
        let requests = state.lock().await.requests.clone();
        server.abort();
        (result, requests)
    }

    #[tokio::test]
    async fn webui_purge_skips_when_neither_value_is_set() {
        let (result, requests) = run_purge(None, None, vec![]).await;
        assert!(!result.unwrap());
        assert!(requests.is_empty());
        let (result, requests) = run_purge(Some(""), Some(""), vec![]).await;
        assert!(!result.unwrap());
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn webui_purge_fails_closed_on_partial_configuration() {
        for (zone, key) in [(Some("123"), None), (None, Some("purge-only-secret"))] {
            let (result, requests) = run_purge(zone, key, vec![]).await;
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("BUNNY_PULL_ZONE_ID and BUNNY_PURGE_API_KEY"));
            assert!(!error.contains("purge-only-secret"));
            assert!(requests.is_empty());
        }
        for zone in ["0", "012", "12/34", "abc"] {
            assert!(purge_env(Some(zone), Some("purge-only-secret")).is_err());
        }
    }

    #[tokio::test]
    async fn webui_purge_posts_scoped_key_and_accepts_ok_or_no_content() {
        for status in [StatusCode::OK, StatusCode::NO_CONTENT] {
            let (result, requests) =
                run_purge(Some("123"), Some("purge-only-secret"), vec![status]).await;
            assert!(result.unwrap());
            assert_eq!(
                requests,
                vec![(
                    "POST /pullzone/123/purgeCache".to_owned(),
                    "purge-only-secret".to_owned(),
                    "{}".to_owned(),
                )]
            );
        }
    }

    #[tokio::test]
    async fn webui_purge_retries_until_success() {
        let failures = vec![
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::ACCEPTED,
            StatusCode::BAD_GATEWAY,
            StatusCode::UNAUTHORIZED,
        ];
        let (result, requests) = run_purge(Some("123"), Some("purge-only-secret"), failures).await;
        assert!(result.unwrap());
        assert_eq!(requests.len(), PURGE_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn webui_purge_fails_after_all_attempts() {
        let failures = vec![StatusCode::SERVICE_UNAVAILABLE; PURGE_ATTEMPTS as usize + 1];
        let (result, requests) = run_purge(Some("123"), Some("purge-only-secret"), failures).await;
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("purge failed after 6 attempts"));
        assert!(!error.contains("purge-only-secret"));
        assert_eq!(requests.len(), PURGE_ATTEMPTS as usize);
    }

    fn purge_values(
        zone: Option<&'static str>,
        key: Option<&'static str>,
    ) -> impl Fn(&str) -> Option<String> {
        move |name| match name {
            "BUNNY_PULL_ZONE_ID" => zone.map(str::to_owned),
            "BUNNY_PURGE_API_KEY" => key.map(str::to_owned),
            _ => None,
        }
    }

    fn purge_test_app() -> (Arc<ApiState>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "jmodguard_webui_purge_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = Db::open(&path, 2).unwrap();
        (ApiState::new(Default::default(), db).unwrap(), path)
    }

    fn remove_test_db(app: Arc<ApiState>, path: std::path::PathBuf) {
        drop(app);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn webui_install_purges_only_after_publication_succeeds() {
        let (app, path) = purge_test_app();
        let (api, state, server) = purge_server(vec![]).await;
        let delay = Duration::from_millis(1);
        let published = std::cell::Cell::new(false);
        // Partial purge configuration fails before publication can touch Storage.
        for (zone, key) in [(Some("123"), None), (None, Some("purge-only-secret"))] {
            let result = publish_then_purge(
                &app,
                purge_values(zone, key),
                async || {
                    published.set(true);
                    Ok(())
                },
                &api,
                delay,
            )
            .await;
            assert!(format!("{:#}", result.unwrap_err()).contains("BUNNY_PULL_ZONE_ID"));
        }
        assert!(!published.get());
        assert!(db::webui_purge_pending(&app.db).await.unwrap().is_none());
        // A publication failure sends no purge and leaves nothing pending.
        let result = publish_then_purge(
            &app,
            purge_values(Some("123"), Some("purge-only-secret")),
            async || -> anyhow::Result<()> { anyhow::bail!("Storage upload failed") },
            &api,
            delay,
        )
        .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("Storage upload failed"));
        assert!(state.lock().await.requests.is_empty());
        assert!(db::webui_purge_pending(&app.db).await.unwrap().is_none());
        // The purge obligation is durable before publication, which completes
        // before the single purge request; the accepted purge clears it.
        let result = publish_then_purge(
            &app,
            purge_values(Some("123"), Some("purge-only-secret")),
            async || {
                assert_eq!(
                    db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
                    Some(PURGE_PENDING_DURING_INSTALL)
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(state.lock().await.requests.is_empty());
                published.set(true);
                Ok("published")
            },
            &api,
            delay,
        )
        .await;
        assert_eq!(result.unwrap(), "published");
        assert!(published.get());
        assert_eq!(state.lock().await.requests.len(), 1);
        assert!(db::webui_purge_pending(&app.db).await.unwrap().is_none());
        // A publication failure keeps an earlier unresolved purge.
        db::set_webui_purge_pending(&app.db, Some("earlier purge failed".into()))
            .await
            .unwrap();
        let result = publish_then_purge(
            &app,
            purge_values(Some("123"), Some("purge-only-secret")),
            async || -> anyhow::Result<()> { anyhow::bail!("Storage upload failed") },
            &api,
            delay,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(state.lock().await.requests.len(), 1);
        assert_eq!(
            db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
            Some("earlier purge failed")
        );
        server.abort();
        remove_test_db(app, path);
    }

    #[tokio::test]
    async fn webui_install_fails_closed_when_the_pending_purge_write_is_rejected() {
        let (app, path) = purge_test_app();
        // A read-only API whose control write is rejected, as by an older bot
        // that does not know SetWebuiPurgePending.
        let mut cfg = (*app.cfg).clone();
        cfg.db.path = path.clone();
        let reader = Db::open_with(cfg.db_open_options()).unwrap();
        assert!(reader.is_read_only());
        let rejecting = ApiState::new(cfg, reader).unwrap();
        let (api, state, server) = purge_server(vec![]).await;
        let published = std::cell::Cell::new(false);
        let error = publish_then_purge(
            &rejecting,
            purge_values(Some("123"), Some("purge-only-secret")),
            async || {
                published.set(true);
                Ok(())
            },
            &api,
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("could not be recorded before publication"));
        assert!(!published.get());
        assert!(state.lock().await.requests.is_empty());
        assert!(db::webui_purge_pending(&app.db).await.unwrap().is_none());
        // Without purge configuration there is no obligation to record.
        publish_then_purge(
            &rejecting,
            purge_values(None, None),
            async || {
                published.set(true);
                Ok(())
            },
            &api,
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert!(published.get());
        assert!(state.lock().await.requests.is_empty());
        server.abort();
        drop(rejecting);
        remove_test_db(app, path);
    }

    /// One hourly pass: the channel lookup returns `channel` or fails, and the
    /// installed Storage files verify with `hashes` or mismatch.
    async fn hourly(
        app: &Arc<ApiState>,
        channel: Option<ComponentRelease>,
        hashes: Option<(String, String)>,
    ) -> ReleaseCheckBatch {
        // Boxed so repeated passes do not grow the calling test's future.
        Box::pin(record_release_checks_with(
            app,
            move |_, _| {
                let channel = channel.clone();
                Box::pin(async move { channel.context("registry returned HTTP 503") })
            },
            RELEASE_LOOKUP_TIMEOUT,
            Box::pin(std::future::pending()),
            async |_, _| {
                Ok(match hashes.clone() {
                    Some((root_hash, admin_hash)) => ReleaseVerification::Verified {
                        root_hash,
                        admin_hash,
                    },
                    None => ReleaseVerification::Mismatch,
                })
            },
        ))
        .await
    }
    fn webui_test_release() -> ComponentRelease {
        ComponentRelease {
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
        }
    }

    #[tokio::test]
    async fn legacy_writer_rejection_keeps_webui_eligible_for_scheduled_install() {
        use jmodguard::control::{NonceWindow, open_frame};
        use jmodguard::db::SqlExecutor;
        use std::sync::Mutex;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (writer, path) = purge_test_app();
        let schema = db::CORE_SCHEMA_VERSION;
        let installed = ComponentRelease {
            channel: "stable".into(),
            min_schema: schema,
            max_schema: schema,
            result_schema: schema,
            ..webui_test_release()
        };
        let now = 100;
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            baked_launcher_revision: 0,
            source_sha: installed.source_sha.clone(),
            executable_sha256: "verified root".into(),
            admin_sha256: Some("verified admin".into()),
            base_image_sha: "Storage Zone".into(),
            started_at: now,
            min_schema: schema,
            max_schema: schema,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "static-webui-v1".into(),
        };
        db::set_update_policy(&writer.db, "webui", "stable", "daily", true)
            .await
            .unwrap();
        db::record_update_check(
            &writer.db,
            "webui",
            "stable",
            Some(installed.clone()),
            None,
            now,
        )
        .await
        .unwrap();
        db::request_component_install(&writer.db, "webui", &installed.digest, now)
            .await
            .unwrap();
        record_running(
            &writer,
            "webui",
            Some(installed.clone()),
            observation.clone(),
            now,
        )
        .await
        .unwrap();
        // A legacy row is readable but no longer serializes to its stored JSON.
        let mut legacy = serde_json::to_value(&installed).unwrap();
        legacy.as_object_mut().unwrap().remove("upgrade");
        let legacy = legacy.to_string();
        let raw_installed = legacy.clone();
        writer
            .db
            .with_conn(move |conn| {
                conn.execute_bounded(
                    "UPDATE component_updates SET installed=?1 WHERE component='webui'",
                    [raw_installed],
                    64 * 1024,
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = (*writer.cfg).clone();
        cfg.db.path = path.clone();
        cfg.bot.control_target = listener.local_addr().unwrap().to_string();
        let key = "x".repeat(32);
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new(key.clone());
        let reader = Db::open_with(cfg.db_open_options()).unwrap();
        assert!(reader.is_read_only());
        let reader = ApiState::new(cfg, reader).unwrap();
        let (rejected, mut rejected_ops) = tokio::sync::mpsc::unbounded_channel();
        let peer = tokio::spawn({
            let writer = Arc::clone(&writer);
            async move {
                let nonces = Mutex::new(NonceWindow::new());
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let length = stream.read_u32().await.unwrap();
                    let mut request = vec![0; length as usize];
                    stream.read_exact(&mut request).await.unwrap();
                    let (op, actor, _, _) = open_frame(key.as_bytes(), &request, &nonces).unwrap();
                    assert_eq!(actor, 0);
                    let reply = match op {
                        // The older writer cannot decode this operation.
                        db::ControlOp::RecordHourlyWebuiDrift { .. } => {
                            rejected.send("mismatch").unwrap();
                            db::ControlReply::Failed {
                                message: "write failed".into(),
                                code: None,
                            }
                        }
                        db::ControlOp::RecordWebuiInstalledVerified { release, .. } => {
                            // The older writer uses serialized JSON equality.
                            assert_ne!(serde_json::to_string(&release).unwrap(), legacy);
                            rejected.send("verified").unwrap();
                            db::ControlReply::Failed {
                                message: "installed WebUI release changed during verification"
                                    .into(),
                                code: None,
                            }
                        }
                        op => db::apply_control(&writer.db, None, op, None).await.unwrap(),
                    };
                    let body = serde_json::to_vec(&reply).unwrap();
                    stream.write_u32(body.len() as u32).await.unwrap();
                    stream.write_all(&body).await.unwrap();
                }
            }
        });

        let candidate = ComponentRelease {
            source_sha: "c".repeat(40),
            digest: format!("sha256:{}", "d".repeat(64)),
            ..installed.clone()
        };
        for (expected_op, hashes) in [
            ("mismatch", None),
            ("verified", Some(("new root".into(), "new admin".into()))),
        ] {
            let batch = hourly(&reader, Some(candidate.clone()), hashes).await;
            assert_eq!(rejected_ops.try_recv().unwrap(), expected_op);
            assert!(batch.finished);
            assert!(!batch.lease_held.contains("webui"), "{expected_op}");
            let checked = db::component_update(&reader.db, "webui").await.unwrap();
            assert_eq!(checked.candidate.as_ref(), Some(&candidate));
            assert_eq!(checked.installed.as_ref(), Some(&installed));
            assert!(checked.check_error.is_none());
            assert_eq!(checked.observed_at, Some(now));
            assert_eq!(checked.state, "running");
        }

        // Give the API a stale local view while the control writer still
        // returns the current WebUI row with the installed digest.
        let (replica, replica_path) = purge_test_app();
        db::set_update_policy(&replica.db, "webui", "stable", "daily", true)
            .await
            .unwrap();
        let mut cfg = (*reader.cfg).clone();
        cfg.db.path = replica_path.clone();
        let db = Db::open_with(cfg.db_open_options()).unwrap();
        let stale_reader = ApiState::new(cfg, db).unwrap();
        let batch = hourly(&stale_reader, Some(candidate.clone()), None).await;
        drop(stale_reader);
        remove_test_db(replica, replica_path);
        assert!(batch.finished);
        assert!(!batch.lease_held.contains("webui"));
        assert_eq!(rejected_ops.try_recv(), Ok("mismatch"));

        // Exercise scheduled admission without publishing to Storage.
        let checked = db::component_update(&reader.db, "webui").await.unwrap();
        let now = checked.checked_at.unwrap();
        record_running(&writer, "api", None, observation, now)
            .await
            .unwrap();
        jmodguard_updater::compatible(&reader.db, &candidate, now)
            .await
            .unwrap();
        let lease = "ab".repeat(16);
        assert_eq!(
            claim_any(Some(&reader), &reader.db, "webui", &lease, now, 600)
                .await
                .unwrap(),
            db::UpdateLeaseStatus::Held
        );
        let queued: Option<ComponentRelease> = commit_typed(
            &reader,
            db::ControlOp::RequestDueAutomaticInstall {
                component: "webui".into(),
                expected_digest: candidate.digest.clone(),
                now,
                unattended_allowed: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(queued, Some(candidate.clone()));
        let row = db::component_update(&reader.db, "webui").await.unwrap();
        assert_eq!(row.desired, Some(candidate));
        assert_eq!(row.state, "queued");
        release_any(Some(&reader), &reader.db, "webui", &lease)
            .await
            .unwrap();
        peer.abort();
        assert!(peer.await.unwrap_err().is_cancelled());
        drop(reader);
        remove_test_db(writer, path);
    }

    #[tokio::test]
    async fn webui_reconciliation_preserves_failed_reinstall_and_adopts_distinct_release() {
        let (app, path) = purge_test_app();
        let installed = webui_test_release();
        let now = 100;
        db::record_update_check(
            &app.db,
            "webui",
            "latest",
            Some(installed.clone()),
            None,
            now,
        )
        .await
        .unwrap();
        db::request_component_install(&app.db, "webui", &installed.digest, now)
            .await
            .unwrap();
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            baked_launcher_revision: 0,
            source_sha: installed.source_sha.clone(),
            executable_sha256: "old root".into(),
            admin_sha256: Some("old admin".into()),
            base_image_sha: "Storage Zone".into(),
            started_at: now - 10,
            min_schema: installed.min_schema,
            max_schema: installed.max_schema,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "static-webui-v1".into(),
        };
        record_running(
            &app,
            "webui",
            Some(installed.clone()),
            observation,
            now - 10,
        )
        .await
        .unwrap();
        db::set_webui_purge_pending(&app.db, Some("purge pending".into()))
            .await
            .unwrap();

        let mut alias = installed.clone();
        alias.channel = "stable".into();
        let mut next = alias.clone();
        next.source_sha = "c".repeat(40);
        next.digest = format!("sha256:{}", "d".repeat(64));
        for (desired, interrupted) in [
            (installed.clone(), false),
            (alias, true),
            (next.clone(), false),
        ] {
            let reinstall = desired.digest == installed.digest;
            db::set_update_policy(&app.db, "webui", &desired.channel, "off", false)
                .await
                .unwrap();
            db::record_update_check(
                &app.db,
                "webui",
                &desired.channel,
                Some(desired.clone()),
                None,
                now,
            )
            .await
            .unwrap();
            db::request_component_install(&app.db, "webui", &desired.digest, now)
                .await
                .unwrap();
            if !interrupted {
                db::record_component_failure(&app.db, "webui", &desired, "publish failed")
                    .await
                    .unwrap();
            }
            let html = format!("/assets/releases/{}/app.js", desired.source_sha).into_bytes();
            let verified = || ReleaseVerification::Verified {
                root_hash: "verified root".into(),
                admin_hash: "verified admin".into(),
            };
            reconcile_webui_with(
                &app,
                true,
                |_| std::future::ready(Ok(Some(html.clone()))),
                |release| {
                    assert!(!reinstall);
                    assert_eq!(release, desired);
                    std::future::ready(Ok(verified()))
                },
                |release| {
                    assert!(reinstall);
                    assert_eq!(release, installed);
                    std::future::ready(Ok(verified()))
                },
            )
            .await
            .unwrap();

            let row = db::component_update(&app.db, "webui").await.unwrap();
            let seen = row.observed.unwrap();
            assert_eq!(seen.executable_sha256, "verified root");
            assert_eq!(seen.admin_sha256.as_deref(), Some("verified admin"));
            assert_eq!(row.desired.as_ref(), Some(&desired));
            if reinstall {
                assert_eq!(row.installed.as_ref(), Some(&installed));
                assert_eq!(row.installed_at, Some(now - 10));
                assert_eq!(seen.started_at, now - 10);
                if interrupted {
                    assert_eq!(row.state, "queued");
                    assert!(row.last_error.is_none());
                    db::settle_interrupted_webui(&app.db, &desired)
                        .await
                        .unwrap();
                    let settled = db::component_update(&app.db, "webui").await.unwrap();
                    assert_eq!(settled.state, "failed");
                    assert!(settled.last_error.unwrap().contains("interrupted"));
                } else {
                    assert_eq!(row.state, "failed");
                    assert_eq!(row.last_error.as_deref(), Some("publish failed"));
                }
            } else {
                assert_eq!(row.state, "running");
                assert_eq!(row.last_error, None);
                assert_eq!(row.installed.as_ref(), Some(&next));
            }
            assert_eq!(
                db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
                Some("purge pending")
            );
        }
        // The channel still points to the installed digest, but discovery
        // refuses its expired signature. The installed readback can still
        // verify it and clear drift.
        db::record_webui_drift(&app.db, "Storage files differ")
            .await
            .unwrap();
        let expired = jmodguard_updater::signature::EXPIRED_IMAGE_SIGNATURE;
        let result = check_one_with(
            &app,
            "webui",
            async |component, channel| {
                assert_eq!(component, "webui");
                assert_eq!(channel, next.channel);
                anyhow::bail!("{expired}");
            },
            async |adopt_desired| {
                assert!(!adopt_desired);
                reconcile_webui_with(
                    &app,
                    adopt_desired,
                    |_| async { unreachable!("installed verifier reads Storage") },
                    |_| async { unreachable!("discovery did not authorize adoption") },
                    |installed| {
                        assert_eq!(installed, next);
                        std::future::ready(Ok(ReleaseVerification::Verified {
                            root_hash: "restored root".into(),
                            admin_hash: "restored admin".into(),
                        }))
                    },
                )
                .await
            },
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), expired);
        let row = db::component_update(&app.db, "webui").await.unwrap();
        assert_eq!(row.check_error.as_deref(), Some(expired));
        assert_eq!(row.state, "running");
        assert_eq!(row.observed.unwrap().executable_sha256, "restored root");
        assert_eq!(row.installed.as_ref(), Some(&next));
        assert_eq!(row.candidate.as_ref(), Some(&next));
        assert_eq!(
            db::request_component_install(&app.db, "webui", &next.digest, row.checked_at.unwrap())
                .await
                .unwrap_err()
                .to_string(),
            "last release check failed; check again",
        );
        assert_eq!(
            db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
            Some("purge pending")
        );
        remove_test_db(app, path);
    }

    #[tokio::test]
    async fn webui_purge_obligation_survives_failed_or_interrupted_purge_through_reconciliation() {
        let release = webui_test_release();
        let observation = ComponentObservation {
            guild_schema: None,
            update_protocol: 0,
            launcher_revision: 0,
            baked_launcher_revision: 0,
            source_sha: release.source_sha.clone(),
            executable_sha256: "c".repeat(64),
            admin_sha256: Some("d".repeat(64)),
            base_image_sha: "Storage Zone".into(),
            started_at: 102,
            min_schema: release.min_schema,
            max_schema: release.max_schema,
            api_capability: 1,
            self_update_enabled: true,
            base_abi: "static-webui-v1".into(),
        };
        // Every attempt fails, or the install is cancelled (as by an API
        // restart) while waiting to retry after its first failed attempt.
        for interrupted in [false, true] {
            let (app, path) = purge_test_app();
            db::record_update_check(
                &app.db,
                "webui",
                &release.channel,
                Some(release.clone()),
                None,
                100,
            )
            .await
            .unwrap();
            let (_, lease) = queue_webui_install(None, &app.db, &release.digest, 101)
                .await
                .unwrap();
            db::release_update_lease(&app.db, "webui", &lease)
                .await
                .unwrap();
            let failures = vec![StatusCode::SERVICE_UNAVAILABLE; PURGE_ATTEMPTS as usize];
            let (api, state, server) = purge_server(failures).await;
            let configured = purge_values(Some("123"), Some("purge-only-secret"));
            let attempts = if interrupted {
                let failed = PurgeAttemptFailed::default();
                let _log = tracing::subscriber::set_default(failed.clone());
                let install = publish_then_purge(
                    &app,
                    &configured,
                    async || Ok(()),
                    &api,
                    Duration::from_secs(3600),
                );
                // The failed-attempt warning is logged just before the retry wait.
                let notified = tokio::select! {
                    _ = install => panic!("install finished instead of waiting to retry"),
                    result = tokio::time::timeout(
                        Duration::from_secs(10),
                        failed.0.notified(),
                    ) => result,
                };
                notified.expect("purge failed-attempt warning was not logged within 10s");
                1
            } else {
                let error = publish_then_purge(
                    &app,
                    &configured,
                    async || Ok(()),
                    &api,
                    Duration::from_millis(1),
                )
                .await
                .unwrap_err();
                // run_webui_install records the failure.
                db::record_component_failure(&app.db, "webui", &release, &format!("{error:#}"))
                    .await
                    .unwrap();
                PURGE_ATTEMPTS as usize
            };
            assert_eq!(state.lock().await.requests.len(), attempts);
            // Storage reconciliation adopts the verified published release.
            record_running(
                &app,
                "webui",
                Some(release.clone()),
                observation.clone(),
                102,
            )
            .await
            .unwrap();
            // The hourly check can verify the installed files through their
            // pinned digest while discovery of the moving tag is unavailable.
            let probe_lease = "ab".repeat(16);
            let batch = Box::pin(record_release_checks_with(
                &app,
                |_, _| Box::pin(async { anyhow::bail!("registry returned HTTP 503") }),
                RELEASE_LOOKUP_TIMEOUT,
                Box::pin(std::future::pending()),
                async |_, installed| {
                    assert_eq!(installed, &release);
                    let checked_at = db::component_update(&app.db, "webui")
                        .await
                        .unwrap()
                        .checked_at
                        .unwrap();
                    assert!(
                        !db::claim_update_lease(&app.db, "webui", &probe_lease, checked_at, 240)
                            .await
                            .unwrap(),
                        "verification must hold the WebUI update lease"
                    );
                    Ok(ReleaseVerification::Verified {
                        root_hash: observation.executable_sha256.clone(),
                        admin_hash: observation.admin_sha256.clone().unwrap(),
                    })
                },
            ))
            .await;
            assert!(batch.finished);
            let row = db::component_update(&app.db, "webui").await.unwrap();
            assert!(
                db::claim_update_lease(
                    &app.db,
                    "webui",
                    &probe_lease,
                    row.checked_at.unwrap(),
                    240,
                )
                .await
                .unwrap(),
                "the hourly check must release the lease"
            );
            db::release_update_lease(&app.db, "webui", &probe_lease)
                .await
                .unwrap();
            assert!(row.observed_at.is_some_and(|at| at > 102));
            assert_eq!(row.installed_at, Some(102));
            assert_eq!(row.observed.unwrap().started_at, observation.started_at);
            assert_eq!(
                row.check_error.as_deref(),
                Some("registry returned HTTP 503")
            );
            assert_eq!(row.state, "running");
            assert!(row.last_error.is_none());
            let pending = db::webui_purge_pending(&app.db).await.unwrap().unwrap();
            if interrupted {
                assert_eq!(pending, PURGE_PENDING_DURING_INSTALL);
            } else {
                assert!(pending.contains("purge failed after 6 attempts"));
            }
            assert!(!pending.contains("purge-only-secret"));
            let status = status_response(&app, None).await.unwrap().0;
            assert_eq!(status["webui_purge_pending"], pending.as_str());
            // A failed readback proves nothing about Storage and keeps the
            // verified observation. A definite mismatch invalidates it.
            let failed = Box::pin(record_release_checks_with(
                &app,
                |_, _| Box::pin(async { anyhow::bail!("registry returned HTTP 503") }),
                RELEASE_LOOKUP_TIMEOUT,
                Box::pin(std::future::pending()),
                async |_, _| anyhow::bail!("Storage read failed"),
            ))
            .await;
            assert!(failed.finished);
            let kept = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(kept.state, "running");
            assert_eq!(kept.observed_at, row.observed_at);
            assert!(kept.observed.is_some());
            let changed = hourly(&app, None, None).await;
            assert!(changed.finished);
            assert!(!changed.lease_held.contains("webui"));
            let drift = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(drift.state, "drift");
            assert!(drift.observed.is_none() && drift.observed_at.is_none());
            assert_eq!(drift.installed.as_ref(), Some(&release));
            assert_eq!(drift.installed_at, Some(102));
            assert_eq!(
                db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
                Some(pending.as_str())
            );
            // An activated install keeps `desired == installed`. Confirmed drift
            // clears once the installed files verify again.
            db::record_update_check(
                &app.db,
                "webui",
                &release.channel,
                Some(release.clone()),
                None,
                104,
            )
            .await
            .unwrap();
            let (_, lease) = queue_webui_install(None, &app.db, &release.digest, 104)
                .await
                .unwrap();
            db::release_update_lease(&app.db, "webui", &lease)
                .await
                .unwrap();
            record_running(
                &app,
                "webui",
                Some(release.clone()),
                observation.clone(),
                105,
            )
            .await
            .unwrap();
            let settled = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(settled.desired.as_ref(), Some(&release));
            assert!(hourly(&app, None, None).await.finished);
            let drift = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(drift.state, "drift");
            let hashes = (
                observation.executable_sha256.clone(),
                observation.admin_sha256.clone().unwrap(),
            );
            assert!(hourly(&app, None, Some(hashes.clone())).await.finished);
            let restored = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(restored.state, "running");
            assert!(restored.last_error.is_none());
            assert!(restored.observed.is_some());
            assert_eq!(restored.installed_at, Some(102));
            assert_eq!(
                db::webui_purge_pending(&app.db).await.unwrap().as_deref(),
                Some(pending.as_str())
            );
            // A reinstall of the installed release keeps its outcome: verified
            // bytes do not show that the attempt completed.
            db::record_update_check(
                &app.db,
                "webui",
                &release.channel,
                Some(release.clone()),
                None,
                106,
            )
            .await
            .unwrap();
            let (_, lease) = queue_webui_install(None, &app.db, &release.digest, 106)
                .await
                .unwrap();
            db::release_update_lease(&app.db, "webui", &lease)
                .await
                .unwrap();
            // A mismatch clears the observation; a verified readback restores
            // it without settling the queued request.
            assert!(hourly(&app, None, None).await.finished);
            let mismatched = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(mismatched.state, "queued");
            assert!(mismatched.observed.is_none());
            assert!(hourly(&app, None, Some(hashes.clone())).await.finished);
            let reinstall = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(reinstall.state, "queued");
            assert_eq!(reinstall.desired.as_ref(), Some(&release));
            assert!(reinstall.observed.is_some());
            db::record_component_failure(&app.db, "webui", &release, "publish failed")
                .await
                .unwrap();
            assert!(hourly(&app, None, Some(hashes.clone())).await.finished);
            let failed_reinstall = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(failed_reinstall.state, "failed");
            assert!(
                failed_reinstall
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("publish failed"))
            );
            assert_eq!(failed_reinstall.desired.as_ref(), Some(&release));
            // A separate failed request keeps its state and error, but a
            // definite mismatch still invalidates the old observation, even
            // when the channel points back at the installed build.
            let mut next = release.clone();
            next.source_sha = "e".repeat(40);
            next.digest = format!("sha256:{}", "e".repeat(64));
            db::record_update_check(
                &app.db,
                "webui",
                &next.channel,
                Some(next.clone()),
                None,
                107,
            )
            .await
            .unwrap();
            let (_, lease) = queue_webui_install(None, &app.db, &next.digest, 107)
                .await
                .unwrap();
            db::release_update_lease(&app.db, "webui", &lease)
                .await
                .unwrap();
            db::settle_interrupted_webui(&app.db, &next).await.unwrap();
            let failed = db::component_update(&app.db, "webui").await.unwrap();
            assert_eq!(failed.state, "failed");
            assert!(failed.observed.is_some());
            assert!(hourly(&app, Some(release.clone()), None).await.finished);
            let invalidated = db::component_update(&app.db, "webui").await.unwrap();
            assert!(invalidated.observed.is_none() && invalidated.observed_at.is_none());
            assert_eq!(invalidated.candidate.as_ref(), Some(&release));
            assert!(invalidated.check_error.is_none());
            assert_eq!(invalidated.state, "failed");
            assert_eq!(invalidated.last_error, failed.last_error);
            assert_eq!(invalidated.desired.as_ref(), Some(&next));
            // A check without purge values cannot resolve it.
            let delay = Duration::from_millis(1);
            retry_pending_purge(&app, purge_values(None, None), &api, delay)
                .await
                .unwrap();
            assert!(db::webui_purge_pending(&app.db).await.unwrap().is_some());
            assert_eq!(state.lock().await.requests.len(), attempts);
            // A check whose purge is accepted clears it.
            state.lock().await.statuses.clear();
            retry_pending_purge(&app, &configured, &api, delay)
                .await
                .unwrap();
            assert!(db::webui_purge_pending(&app.db).await.unwrap().is_none());
            assert_eq!(state.lock().await.requests.len(), attempts + 1);
            let status = status_response(&app, None).await.unwrap().0;
            assert!(status["webui_purge_pending"].is_null());
            server.abort();
            remove_test_db(app, path);
        }
    }

    #[test]
    fn scheduled_auto_install_allows_only_promoted_cadences() {
        for cadence in ["daily", "weekly", "monthly"] {
            assert!(scheduled_auto_install_allowed("stable", cadence));
            assert!(scheduled_auto_install_allowed("nightly", cadence));
        }
        for channel in ["latest", "stable", "nightly"] {
            assert!(!scheduled_auto_install_allowed(channel, "off"));
        }
        for cadence in ["daily", "weekly", "monthly"] {
            assert!(!scheduled_auto_install_allowed("latest", cadence));
        }
    }

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
        with_update_lease(None, &db, "webui", &first, 4, async {
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
            with_update_lease(None, &db, "webui", &first, 4, async {
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
        let release = webui_test_release();
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
            queue_webui_install(None, &db, &release.digest, 101)
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
        let (queued, lease) = queue_webui_install(None, &db, &release.digest, 102)
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

    #[tokio::test]
    async fn shown_policy_uses_the_write_row_when_a_snapshot_is_stale() {
        use jmodguard::db::SqlExecutor;
        let path = std::env::temp_dir().join(format!(
            "jmodguard_replica_read_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = jmodguard::db::Db::open(&path, 2).unwrap();
        let before = db::component_update(&db, "api").await.unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let reader = db.clone();
        let held = tokio::spawn(async move {
            reader
                .with_conn(move |conn| {
                    let tx = conn.transaction_primary_snapshot()?;
                    let stale: String = tx.query_row(
                        "SELECT cadence FROM component_updates WHERE component='api'",
                        (),
                        |row| Ok(row.get(0)?),
                    )?;
                    let _ = ready_tx.send(());
                    let _ = release_rx.recv();
                    let still: String = tx.query_row(
                        "SELECT cadence FROM component_updates WHERE component='api'",
                        (),
                        |row| Ok(row.get(0)?),
                    )?;
                    tx.finish_read()?;
                    Ok((stale, still))
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx)
            .await
            .expect("snapshot reader did not start")
            .unwrap();
        let written = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            db::set_update_policy(&db, "api", "stable", "daily", true),
        )
        .await
        .expect("the write must not wait on the stale read")
        .unwrap();
        let _ = release_tx.send(());
        let (stale, still) = held.await.unwrap().unwrap();
        assert_eq!(stale, before.cadence);
        assert_eq!(
            still, before.cadence,
            "a read started before the write must not observe that commit"
        );
        assert_ne!(written.cadence, still);
        let shown = super::overlay_written_component(vec![before], &written);
        assert_eq!(shown[0].channel, "stable");
        assert_eq!(shown[0].cadence, "daily");
        assert_eq!(shown[0].cadence, written.cadence);
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
