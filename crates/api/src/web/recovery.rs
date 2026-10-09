//! Owner update control that never opens tenant/application state. The baked
//! API can serve this same page after a newer application schema is installed.
use super::*;
use crate::config::ApiProcessConfig;
use crate::db::{self, UpdateControl};
use anyhow::Context as _;
use axum::extract::State;
use axum::response::Html;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct RecoveryState {
    pub cfg: Arc<ApiProcessConfig>,
    pub oauth: Arc<oauth::OAuthClient>,
    pub control: UpdateControl,
    pub apply_enabled: bool,
}

async fn recovery_value<T>(
    local: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    local.await
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn problem(status: StatusCode, message: impl ToString) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message.to_string()})))
}

async fn owner(state: &RecoveryState, session: &Session) -> Result<(), (StatusCode, Json<Value>)> {
    let uid = session
        .get::<u64>(SESSION_USER)
        .await
        .map_err(|_| problem(StatusCode::UNAUTHORIZED, "sign in again"))?;
    if uid.is_none() {
        return Err(problem(
            StatusCode::UNAUTHORIZED,
            "sign in to manage service upgrades",
        ));
    }
    if state.cfg.bot.owner_id == 0 || uid != Some(state.cfg.bot.owner_id) {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "only the configured owner can manage upgrades",
        ));
    }
    Ok(())
}

async fn mutation(
    state: &RecoveryState,
    session: &Session,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    owner(state, session).await?;
    if !trusted_web_origin(&state.cfg, headers) {
        return Err(problem(StatusCode::FORBIDDEN, "untrusted request origin"));
    }
    Ok(())
}

pub fn routes<S: Clone + Send + Sync + 'static>(state: Arc<RecoveryState>) -> Router<S> {
    Router::new()
        .route(
            "/api/service-upgrade/recovery",
            get(|| async { Html(include_str!("recovery.html")) }),
        )
        .route("/api/service-upgrade", get(status).post(apply))
        .route("/api/service-upgrade/check", axum::routing::post(check))
        .route("/api/service-upgrade/cancel", axum::routing::post(cancel))
        .with_state(state)
}

async fn status(State(state): State<Arc<RecoveryState>>, session: Session) -> Reply {
    owner(&state, &session).await?;
    let control = &state.control;
    let result = async {
        Ok::<_, anyhow::Error>(json!({
            "operation":control.upgrade().await?,
            "launchers":control.components().await?.into_iter().filter(|row| row.component != "webui").map(|row| (row.component.clone(), jmodguard_updater::launcher_status(&row, chrono::Utc::now().timestamp()))).collect::<serde_json::Map<String, Value>>(),
            "components":control.components().await?.into_iter().filter(|row| row.component != "webui").collect::<Vec<_>>(),
            "core_schema":control.schema_version().await?,
            "apply_enabled":state.apply_enabled,
        }))
    }.await.map_err(|_| problem(StatusCode::SERVICE_UNAVAILABLE, "update control unavailable"))?;
    Ok(Json(result))
}

#[derive(Deserialize)]
struct Check {
    channel: String,
}
async fn check(
    State(state): State<Arc<RecoveryState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<Check>,
) -> Reply {
    mutation(&state, &session, &headers).await?;
    if !matches!(input.channel.as_str(), "latest" | "stable" | "nightly") {
        return Err(problem(StatusCode::BAD_REQUEST, "invalid channel"));
    }
    let now = chrono::Utc::now().timestamp();
    // Same lease as Administration Check now, held for both services so an
    // hourly poll cannot interleave. Registry work happens before any row write.
    let owner = recovery_value(state.control.acquire_pair_check(now))
        .await
        .map_err(|e| {
            problem(
                StatusCode::CONFLICT,
                format!("matching pair unavailable: {e:#}"),
            )
        })?;
    let result = async {
        let registry = jmodguard_updater::oci::Registry::ghcr()?;
        let bot = registry.discover("bot", &input.channel).await?;
        let api = registry
            .discover_source("api", &input.channel, &bot.source_sha)
            .await?;
        // Both images must resolve before either channel or candidate changes.
        // One transaction commits the pair; a missing counterpart or a failed
        // second write leaves the previous selection in place.
        recovery_value(
            state
                .control
                .record_pair_check(&owner, &input.channel, bot, api, now),
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = recovery_value(state.control.release_pair_check(&owner)).await {
        tracing::warn!("paired check lease release failed: {error:#}");
    }
    result.map_err(|e| {
        problem(
            StatusCode::CONFLICT,
            format!("matching pair unavailable: {e:#}"),
        )
    })?;
    status(State(state), session).await
}

#[derive(Deserialize)]
struct Apply {
    bot_digest: String,
    api_digest: String,
    confirmed: bool,
    #[serde(default)]
    recover: bool,
}
async fn apply(
    State(state): State<Arc<RecoveryState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<Apply>,
) -> Reply {
    mutation(&state, &session, &headers).await?;
    if !input.confirmed || !state.apply_enabled {
        return Err(problem(
            StatusCode::CONFLICT,
            "explicit confirmation and enabled on-demand updates are required",
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let operation = recovery_value(state.control.request_upgrade(
        &input.bot_digest,
        &input.api_digest,
        input.recover,
        now,
    ))
    .await
    .map_err(|e| problem(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({"operation":operation})))
}

#[derive(Deserialize)]
struct Cancel {
    id: String,
}
async fn cancel(
    State(state): State<Arc<RecoveryState>>,
    session: Session,
    headers: HeaderMap,
    Json(input): Json<Cancel>,
) -> Reply {
    mutation(&state, &session, &headers).await?;
    recovery_value(state.control.abort_upgrade(&input.id))
        .await
        .map_err(|e| problem(StatusCode::CONFLICT, format!("{e:#}")))?;
    status(State(state), session).await
}

async fn maintenance_login(
    State(state): State<Arc<RecoveryState>>,
    Query(query): Query<ReturnToQuery>,
) -> Response {
    login_response(&state.cfg, &state.oauth, query)
}
async fn maintenance_callback(
    State(state): State<Arc<RecoveryState>>,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    callback_response(session, &state.cfg, &state.oauth, headers, query).await
}

/// Bound and healthy during control-store startup, without constructing ApiState
/// or running core/guild application migrations. Only the update routes exist.
pub async fn serve(cfg: ApiProcessConfig) -> anyhow::Result<()> {
    crate::set_shutdown_context("temporary maintenance API; the application has not started");
    let listener = crate::health::bind(&cfg.web.bind).await?;
    let mut opts = cfg.db_open_options();
    opts.read_only = false;
    let control = crate::health::serve_while(&listener, async {
        loop {
            let opts = opts.clone();
            match tokio::task::spawn_blocking(move || UpdateControl::open(&opts)).await? {
                Ok(Some(control)) => break Ok::<_, anyhow::Error>(Some(control)),
                Ok(None) => {
                    tracing::warn!("maintenance update control is not ready: database is empty")
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "maintenance update control could not be opened writable: {error:#}"
                    ));
                }
            }
            tokio::select! {
                _ = crate::shutdown_signal() => {
                    tracing::info!(
                        "temporary maintenance API stopped; the application has not started"
                    );
                    return Ok(None);
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
            }
        }
    })
    .await?;
    let Some(control) = control else {
        crate::log_shutdown_finished("temporary maintenance API");
        return Ok(());
    };
    let oauth = Arc::new(oauth::OAuthClient::from_config(&cfg)?);
    let state = Arc::new(RecoveryState {
        cfg: Arc::new(cfg),
        oauth,
        control,
        apply_enabled: std::env::var("JMODGUARD_UPDATE_APPLY_ENABLED").as_deref() == Ok("1"),
    });
    let app = maintenance_router(state, MemoryStore::default());
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal())
        .await?;
    crate::log_shutdown_finished("temporary maintenance API");
    Ok(())
}

async fn maintenance_ready() -> (StatusCode, &'static str) {
    (StatusCode::SERVICE_UNAVAILABLE, "not ready")
}

fn maintenance_router(state: Arc<RecoveryState>, store: MemoryStore) -> Router {
    let (secure, same_site) = public_cookie_policy(&state.cfg);
    let app = Router::new()
        .route("/login", get(maintenance_login))
        .route("/oauth/callback", get(maintenance_callback))
        .with_state(state.clone())
        .merge(routes(state))
        .route(
            "/",
            get(|| async { axum::response::Redirect::to("/api/service-upgrade/recovery") }),
        )
        .route("/health", get(health))
        .route("/health/", get(health))
        .route("/ready", get(maintenance_ready))
        .route("/ready/", get(maintenance_ready))
        .layer(
            SessionManagerLayer::new(store)
                .with_name(SESSION_COOKIE_NAME)
                .with_secure(secure)
                .with_same_site(same_site)
                .with_expiry(Expiry::OnInactivity(CookieDuration::days(7))),
        );
    uncacheable_response_headers(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, ComponentObservation, ComponentRelease, Db, UpgradeCompatibility};
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn cookie(store: &MemoryStore, uid: u64) -> String {
        let session = Session::new(None, Arc::new(store.clone()), None);
        session.insert(SESSION_USER, uid).await.unwrap();
        session.save().await.unwrap();
        format!("{SESSION_COOKIE_NAME}={}", session.id().unwrap())
    }

    #[tokio::test]
    async fn future_schema_recovery_requires_owner_origin_and_explicit_pair_confirmation() {
        let path =
            std::env::temp_dir().join(format!("recovery-http-{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        db.with_conn(|conn| {
            conn.pragma_update(None, "user_version", db::CORE_SCHEMA_VERSION + 1)?;
            Ok(())
        })
        .await
        .unwrap();
        let mut cfg = ApiProcessConfig::default();
        cfg.db.path = path.clone();
        cfg.db.pool_size = 2;
        cfg.bot.owner_id = 123;
        cfg.web.base_url = "https://api.example.test".into();
        assert!(Db::open_with(cfg.db_open_options()).is_err());
        let mut writable = cfg.db_open_options();
        writable.read_only = false;
        let control = UpdateControl::open(&writable).unwrap().unwrap();
        assert!(control.writes_locally());
        let now = chrono::Utc::now().timestamp();
        for (role, digit) in [("bot", 'a'), ("api", 'b')] {
            let release = ComponentRelease {
                component: role.into(),
                channel: "latest".into(),
                release_id: "test".into(),
                source_sha: "c".repeat(40),
                published_at: "2026-10-01T00:00:00Z".into(),
                image: format!("ghcr.io/jatmn/jmodguard-{role}"),
                digest: format!("sha256:{}", digit.to_string().repeat(64)),
                min_schema: db::CORE_SCHEMA_VERSION + 2,
                max_schema: db::CORE_SCHEMA_VERSION + 2,
                result_schema: db::CORE_SCHEMA_VERSION + 2,
                min_api: 1,
                max_api: 1,
                launcher_protocol: 1,
                launcher: Some(db::LauncherCompatibility {
                    revision: db::LAUNCHER_REVISION,
                    min_revision: db::REQUIRED_LAUNCHER_REVISION,
                }),
                base_abi: "debian-bookworm-amd64".into(),
                upgrade: Some(UpgradeCompatibility {
                    control_protocol: db::UPDATE_CONTROL_PROTOCOL,
                    min_core: 15,
                    min_guild: 1,
                    guild_schema: db::guild_data::SCHEMA_VERSION + 1,
                }),
            };
            control
                .checked(role, "latest", Some(release), None, now)
                .await
                .unwrap();
            control
                .running(
                    role,
                    None,
                    ComponentObservation {
                        source_sha: "d".repeat(40),
                        executable_sha256: "e".repeat(64),
                        admin_sha256: None,
                        base_image_sha: "d".repeat(40),
                        started_at: now,
                        min_schema: db::CORE_SCHEMA_VERSION + 1,
                        max_schema: db::CORE_SCHEMA_VERSION + 1,
                        api_capability: 1,
                        self_update_enabled: true,
                        base_abi: "debian-bookworm-amd64".into(),
                        guild_schema: Some(db::guild_data::SCHEMA_VERSION),
                        update_protocol: db::UPDATE_CONTROL_PROTOCOL,
                        launcher_revision: 0,
                    },
                    now,
                )
                .await
                .unwrap();
        }
        let state = Arc::new(RecoveryState {
            oauth: Arc::new(oauth::OAuthClient::from_config(&cfg).unwrap()),
            cfg: Arc::new(cfg),
            control: control.clone(),
            apply_enabled: true,
        });
        let store = MemoryStore::default();
        let owner_cookie = cookie(&store, 123).await;
        let other_cookie = cookie(&store, 456).await;
        let router = maintenance_router(state, store);
        let input = json!({"bot_digest":format!("sha256:{}", "a".repeat(64)), "api_digest":format!("sha256:{}", "b".repeat(64)), "confirmed":true});
        for (session, origin, expected) in [
            ("", "https://api.example.test", StatusCode::UNAUTHORIZED),
            (
                other_cookie.as_str(),
                "https://api.example.test",
                StatusCode::FORBIDDEN,
            ),
            (owner_cookie.as_str(), "", StatusCode::FORBIDDEN),
            (
                owner_cookie.as_str(),
                "https://evil.example",
                StatusCode::FORBIDDEN,
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/service-upgrade")
                        .header("cookie", session)
                        .header("origin", origin)
                        .header("content-type", "application/json")
                        .body(Body::from(input.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            assert_uncacheable_headers(response.headers());
            assert!(control.upgrade().await.unwrap().is_none());
        }
        for confirmed in [false, true] {
            let mut input = input.clone();
            input["confirmed"] = json!(confirmed);
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/service-upgrade")
                        .header("cookie", &owner_cookie)
                        .header("origin", "https://api.example.test")
                        .header("content-type", "application/json")
                        .body(Body::from(input.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if confirmed {
                    StatusCode::OK
                } else {
                    StatusCode::CONFLICT
                }
            );
        }
        assert!(control.upgrade().await.unwrap().is_some());
        let upgrade_id = control.upgrade().await.unwrap().unwrap().id;
        db.with_conn(|conn| {
            let raw: String = conn.query_row(
                "SELECT value FROM meta WHERE key='component_upgrade_v1'",
                (),
                |row| Ok(row.get(0)?),
            )?;
            let mut value: serde_json::Value = serde_json::from_str(&raw)?;
            value["irreversible"] = serde_json::json!(true);
            let mut tx = conn.transaction_immediate()?;
            tx.execute_bounded(
                "UPDATE meta SET value=?1 WHERE key='component_upgrade_v1'",
                db::params![value.to_string()],
                65_536,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
        let cancel = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/service-upgrade/cancel")
                    .header("cookie", &owner_cookie)
                    .header("origin", "https://api.example.test")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id": upgrade_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::CONFLICT);
        // The HTML and OAuth return both use the existing public /api* proxy.
        let page = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/service-upgrade/recovery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert!(
            page.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        // Maintenance serves no application/guild routes, but shares OAuth's
        // CSRF cookies and redirect allowlist rather than an alternate secret.
        assert_eq!(
            router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/guilds")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/login?return_to=%2Fapi%2Fservice-upgrade%2Frecovery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response
                .headers()
                .get_all("set-cookie")
                .iter()
                .any(|value| value.to_str().unwrap().contains(COOKIE_OAUTH_STATE))
        );
        assert!(
            response
                .headers()
                .get_all("set-cookie")
                .iter()
                .any(|value| value
                    .to_str()
                    .unwrap()
                    .contains("jmodguard_oauth_return_to=/api/service-upgrade/recovery"))
        );
        drop((control, db));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
