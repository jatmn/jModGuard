//! Owner update control that never opens tenant/application state. The baked
//! API can serve this same page after a newer application schema is installed.
use super::*;
use crate::config::ApiProcessConfig;
use crate::db::UpdateControl;
use axum::extract::State;
use axum::response::Html;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct RecoveryState {
    pub cfg: Arc<ApiProcessConfig>,
    pub oauth: Arc<oauth::OAuthClient>,
    pub control: UpdateControl,
    pub apply_enabled: bool,
    /// Normal router sends owner actions through `api`. Maintenance `serve`
    /// stays read-only and uses `writer` when the control target is set.
    pub api: Option<Arc<super::state::ApiState>>,
    pub writer: Option<jmodguard::control::WriterLink>,
}

async fn control_value<T: serde::de::DeserializeOwned>(
    state: &RecoveryState,
    local: impl std::future::Future<Output = anyhow::Result<T>>,
    op: crate::db::ControlOp,
) -> anyhow::Result<T> {
    if state.control.writes_locally() {
        return local.await;
    }
    if let Some(api) = state.api.as_ref() {
        let (_, body) = api.commit_json(op).await?;
        return Ok(serde_json::from_value(body)?);
    }
    let Some(writer) = state.writer.clone() else {
        anyhow::bail!("{}", jmodguard::control::CONTROL_LINK_UNAVAILABLE);
    };
    let reply = writer.submit(op, 0).await?;
    let body = match reply {
        crate::db::ControlReply::Saved { body } | crate::db::ControlReply::Unchanged { body } => {
            body
        }
        crate::db::ControlReply::Failed { message, .. } => anyhow::bail!(message),
        crate::db::ControlReply::Waiting => anyhow::bail!(jmodguard::control::NOT_THE_WRITER),
        crate::db::ControlReply::Busy => {
            anyhow::bail!(jmodguard::control::CONTROL_LINK_UNAVAILABLE)
        }
        crate::db::ControlReply::StillWaiting => {
            anyhow::bail!(jmodguard::control::OUTCOME_UNKNOWN)
        }
    };
    Ok(serde_json::from_value(body)?)
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;
fn problem(status: StatusCode, message: impl ToString) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error":message.to_string()})))
}

fn owner_error(failure: ApiError) -> (StatusCode, Json<Value>) {
    match failure {
        failure if failure.0.is::<SessionStoreUnavailable>() => {
            problem(StatusCode::SERVICE_UNAVAILABLE, SessionStoreUnavailable)
        }
        failure if failure.0.is::<AuthLost>() => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"discord authorization failed", "code":"auth_lost"})),
        ),
        failure if failure.0.is::<NotAuthenticated>() => problem(
            StatusCode::UNAUTHORIZED,
            "sign in to manage service upgrades",
        ),
        _ => problem(
            StatusCode::FORBIDDEN,
            "only the configured owner can manage upgrades",
        ),
    }
}

async fn owner(state: &RecoveryState, session: &Session) -> Result<(), (StatusCode, Json<Value>)> {
    if let Some(api) = state.api.as_ref() {
        return session_owner(api, session)
            .await
            .map(|_| ())
            .map_err(owner_error);
    }
    // Maintenance has its own fresh MemoryStore and no production retirement
    // paths or coordinator/tombstone state. Keep its OAuth owner check intact.
    let uid = session
        .get::<u64>(SESSION_USER)
        .await
        .map_err(|_| problem(StatusCode::SERVICE_UNAVAILABLE, SessionStoreUnavailable))?;
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
    let owner = control_value(
        &state,
        state.control.acquire_pair_check(now),
        crate::db::ControlOp::AcquirePairCheck { now },
    )
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
        control_value(
            &state,
            state
                .control
                .record_pair_check(&owner, &input.channel, bot.clone(), api.clone(), now),
            crate::db::ControlOp::RecordPairCheck {
                owner: owner.clone(),
                channel: input.channel.clone(),
                bot,
                api,
                now,
            },
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = control_value(
        &state,
        state.control.release_pair_check(&owner),
        crate::db::ControlOp::ReleasePairCheck {
            owner: owner.clone(),
        },
    )
    .await
    {
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
    let operation = control_value(
        &state,
        state
            .control
            .request_upgrade(&input.bot_digest, &input.api_digest, input.recover, now),
        crate::db::ControlOp::RequestUpgrade {
            bot_digest: input.bot_digest.clone(),
            api_digest: input.api_digest.clone(),
            recover: input.recover,
            now,
        },
    )
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
    control_value(
        &state,
        state.control.abort_upgrade(&input.id),
        crate::db::ControlOp::AbortUpgrade {
            id: input.id.clone(),
        },
    )
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
    callback_response(session, &state.cfg, &state.oauth, None, headers, query).await
}

/// Bound and healthy during control-store startup, without constructing ApiState
/// or running core/guild application migrations. Only the update routes exist.
pub async fn serve(cfg: ApiProcessConfig) -> anyhow::Result<()> {
    crate::set_shutdown_context("temporary maintenance API; the application has not started");
    let listener = crate::health::bind(&cfg.web.bind).await?;
    let opts = cfg.db_open_options();
    let control = crate::health::serve_while(&listener, async {
        loop {
            let opts = opts.clone();
            match tokio::task::spawn_blocking(move || UpdateControl::open(&opts)).await? {
                Ok(Some(control)) => break Ok::<_, anyhow::Error>(Some(control)),
                Ok(None) => {
                    tracing::warn!("maintenance update control is not ready: database is empty")
                }
                Err(error) => {
                    tracing::warn!("maintenance update control is not ready: {error:#}");
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
    let writer = jmodguard::control::try_writer_link(
        cfg.bot.control_target.as_str(),
        cfg.bot.control_secret.as_str(),
    )
    .ok()
    .flatten();
    if writer.is_none() {
        tracing::warn!("control link unavailable");
    }
    let state = Arc::new(RecoveryState {
        cfg: Arc::new(cfg),
        oauth,
        control,
        apply_enabled: std::env::var("JMODGUARD_UPDATE_APPLY_ENABLED").as_deref() == Ok("1"),
        api: None,
        writer,
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
    use crate::db::{
        self, ComponentObservation, ComponentRelease, Db, SqlExecutor, UpgradeCompatibility,
    };
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    async fn cookie(store: &MemoryStore, uid: u64) -> String {
        let session = Session::new(None, Arc::new(store.clone()), None);
        session.insert(SESSION_USER, uid).await.unwrap();
        session.save().await.unwrap();
        format!("{SESSION_COOKIE_NAME}={}", session.id().unwrap())
    }

    #[derive(Clone, Debug, Default)]
    struct FailingStore {
        inner: MemoryStore,
        fail_load: Arc<std::sync::atomic::AtomicBool>,
        fail_delete: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl tower_sessions::SessionStore for FailingStore {
        async fn create(
            &self,
            record: &mut tower_sessions::session::Record,
        ) -> tower_sessions::session_store::Result<()> {
            self.inner.create(record).await
        }

        async fn save(
            &self,
            record: &tower_sessions::session::Record,
        ) -> tower_sessions::session_store::Result<()> {
            self.inner.save(record).await
        }

        async fn load(
            &self,
            id: &tower_sessions::session::Id,
        ) -> tower_sessions::session_store::Result<Option<tower_sessions::session::Record>>
        {
            if self.fail_load.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(tower_sessions::session_store::Error::Backend(
                    "private backend detail: not authenticated".into(),
                ));
            }
            self.inner.load(id).await
        }

        async fn delete(
            &self,
            id: &tower_sessions::session::Id,
        ) -> tower_sessions::session_store::Result<()> {
            if self.fail_delete.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(tower_sessions::session_store::Error::Backend(
                    "private backend detail: delete failed".into(),
                ));
            }
            self.inner.delete(id).await
        }
    }

    async fn request(
        router: &Router,
        method: &str,
        route: &str,
        cookie: &str,
        input: &str,
    ) -> (StatusCode, Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(route)
                    .header("cookie", cookie)
                    .header("origin", "https://api.example.test")
                    .header("content-type", "application/json")
                    .body(Body::from(input.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn recovery_session_store_errors_are_generic_and_retryable() {
        use std::sync::atomic::Ordering::SeqCst;
        let path =
            std::env::temp_dir().join(format!("recovery-store-{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let mut cfg = ApiProcessConfig::default();
        cfg.bot.owner_id = 123;
        cfg.web.base_url = "https://api.example.test".into();
        let app = ApiState::new(cfg, db.clone()).unwrap();
        for maintenance in [false, true] {
            let state = Arc::new(RecoveryState {
                cfg: app.cfg.clone(),
                oauth: app.oauth.clone(),
                control: UpdateControl::from_application(&db),
                apply_enabled: false,
                api: (!maintenance).then(|| app.clone()),
                writer: None,
            });
            for scenario in ["missing", "nonowner", "load", "retired", "dead"] {
                if maintenance && matches!(scenario, "retired" | "dead") {
                    continue;
                }
                for (method, route, input) in [
                    ("GET", "/api/service-upgrade", ""),
                    (
                        "POST",
                        "/api/service-upgrade",
                        r#"{"bot_digest":"","api_digest":"","confirmed":false}"#,
                    ),
                    (
                        "POST",
                        "/api/service-upgrade/check",
                        r#"{"channel":"invalid"}"#,
                    ),
                    ("POST", "/api/service-upgrade/cancel", r#"{"id":""}"#),
                    // These consumers must preserve the same store-error contract.
                    ("GET", "/test/updates", ""),
                    ("GET", "/test/usage", ""),
                    ("GET", "/test/global", ""),
                ] {
                    if (maintenance || matches!(scenario, "missing" | "nonowner"))
                        && route.starts_with("/test/")
                    {
                        continue;
                    }
                    let store = FailingStore::default();
                    let mut cookie =
                        cookie(&store.inner, if scenario == "nonowner" { 456 } else { 123 }).await;
                    let id = cookie.split_once('=').unwrap().1.parse().unwrap();
                    match scenario {
                        "missing" => cookie.clear(),
                        "nonowner" => {}
                        "load" => store.fail_load.store(true, SeqCst),
                        "retired" => {
                            app.retired_auth_sessions
                                .insert(id, std::time::Instant::now());
                            store.fail_delete.store(true, SeqCst);
                        }
                        "dead" => {
                            app.auth_sessions.insert(
                                id,
                                Arc::new(tokio::sync::Mutex::new(SessionAuth {
                                    dead: true,
                                    ..Default::default()
                                })),
                            );
                            store.fail_delete.store(true, SeqCst);
                        }
                        _ => unreachable!(),
                    }
                    let consumers = Router::new()
                        .route("/test/updates", get(updates::status))
                        .route("/test/usage", get(usage::overview))
                        .route(
                            "/test/global",
                            get(
                                |State(app): State<Arc<ApiState>>, session: Session| async move {
                                    guild_scope(&app, &session, 0)
                                        .await
                                        .map(|_| Json(json!({"ok":true})))
                                },
                            ),
                        )
                        .with_state(app.clone());
                    let router = routes(state.clone()).merge(consumers).layer(
                        SessionManagerLayer::new(store.clone()).with_name(SESSION_COOKIE_NAME),
                    );
                    let (status, body) = request(&router, method, route, &cookie, input).await;
                    if matches!(scenario, "missing" | "nonowner") {
                        let (expected_status, message) = if scenario == "missing" {
                            (
                                StatusCode::UNAUTHORIZED,
                                "sign in to manage service upgrades",
                            )
                        } else {
                            (
                                StatusCode::FORBIDDEN,
                                "only the configured owner can manage upgrades",
                            )
                        };
                        assert_eq!(
                            status, expected_status,
                            "{maintenance} {method} {route}: {body}"
                        );
                        assert_eq!(body, json!({"error":message}));
                        continue;
                    }
                    assert_eq!(
                        status,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "{maintenance} {scenario} {method} {route}: {body}"
                    );
                    assert_eq!(body, json!({"error":"session store unavailable"}));
                    // A transient backend failure must not destroy the stored identity
                    // or bypass a retirement/dead-session rejection when retried.
                    store.fail_load.store(false, SeqCst);
                    store.fail_delete.store(false, SeqCst);
                    let (status, body) = if scenario == "load" {
                        request(&router, "GET", "/api/service-upgrade", &cookie, "").await
                    } else {
                        request(&router, method, route, &cookie, input).await
                    };
                    if scenario == "load" {
                        assert_eq!(status, StatusCode::OK, "{body}");
                    } else {
                        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
                        assert_eq!(
                            body,
                            json!({"error":"discord authorization failed", "code":"auth_lost"})
                        );
                    }
                }
            }
        }
        drop((app, db));
        std::fs::remove_file(&path).unwrap();
        crate::tenant::remove_sidecars(&path);
    }

    #[tokio::test]
    async fn require_auth_session_failures_are_generic_and_retryable() {
        use std::sync::atomic::Ordering::SeqCst;
        let path =
            std::env::temp_dir().join(format!("auth-store-{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        let app = ApiState::new(ApiProcessConfig::default(), db).unwrap();
        for scenario in ["load", "dead", "retired", "missing", "token", "refresh"] {
            let store = FailingStore::default();
            let session = Session::new(None, Arc::new(store.clone()), None);
            session.insert(SESSION_USER, 123_u64).await.unwrap();
            session.insert(SESSION_TOKEN, "token").await.unwrap();
            if matches!(scenario, "token" | "refresh") {
                // A malformed stored value must not be mistaken for an absent credential.
                let key = if scenario == "token" {
                    SESSION_TOKEN
                } else {
                    SESSION_REFRESH
                };
                session.insert(key, 123_u64).await.unwrap();
            }
            session.save().await.unwrap();
            let id = session.id().unwrap();
            let cookie = if scenario == "missing" {
                String::new()
            } else {
                format!("{SESSION_COOKIE_NAME}={id}")
            };
            match scenario {
                "load" => store.fail_load.store(true, SeqCst),
                "dead" => {
                    app.auth_sessions.insert(
                        id,
                        Arc::new(tokio::sync::Mutex::new(SessionAuth {
                            dead: true,
                            ..Default::default()
                        })),
                    );
                    store.fail_delete.store(true, SeqCst);
                }
                "retired" => {
                    app.retired_auth_sessions
                        .insert(id, std::time::Instant::now());
                    store.fail_delete.store(true, SeqCst);
                }
                _ => {}
            }
            let router = Router::new()
                .route("/test/auth", get(api_me))
                .with_state(app.clone())
                .layer(SessionManagerLayer::new(store.clone()).with_name(SESSION_COOKIE_NAME));
            let (status, body) = request(&router, "GET", "/test/auth", &cookie, "").await;
            if scenario == "missing" {
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
                assert_eq!(body, json!({"error":"not authenticated"}));
                continue;
            }
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{scenario}: {body}"
            );
            assert_eq!(body, json!({"error":"session store unavailable"}));
            store.fail_load.store(false, SeqCst);
            store.fail_delete.store(false, SeqCst);
            if matches!(scenario, "dead" | "retired") {
                let (status, body) = request(&router, "GET", "/test/auth", &cookie, "").await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
                assert_eq!(body["code"], "auth_lost");
            } else {
                // Read failures preserve the stored identity for a retry.
                let retry = Session::new(Some(id), Arc::new(store), None);
                assert_eq!(retry.get::<u64>(SESSION_USER).await.unwrap(), Some(123));
            }
        }
        drop(app);
        std::fs::remove_file(&path).unwrap();
        crate::tenant::remove_sidecars(&path);
    }

    #[test]
    fn owner_error_uses_error_types_not_message_text() {
        assert_eq!(NotAuthenticated.to_string(), "not authenticated");
        let typed = anyhow::Error::new(NotAuthenticated).context("session unavailable");
        assert_eq!(typed.to_string(), "session unavailable");
        let (status, body) = owner_error(ApiError(typed));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body.0,
            json!({"error":"sign in to manage service upgrades"})
        );

        let (status, body) = owner_error(ApiError(
            anyhow::Error::new(AuthLost).context("not authenticated"),
        ));
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            body.0,
            json!({"error":"discord authorization failed", "code":"auth_lost"})
        );

        for message in [
            "not authenticated",
            "database unavailable",
            "temporary failure",
        ] {
            let (status, body) = owner_error(ApiError(anyhow::anyhow!(message)));
            assert_eq!(status, StatusCode::FORBIDDEN, "{message}");
            assert_eq!(
                body.0,
                json!({"error":"only the configured owner can manage upgrades"})
            );
        }
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
            api: None,
            writer: None,
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
            assert_eq!(response.status(), StatusCode::CONFLICT);
            if confirmed {
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                let text = String::from_utf8(body.to_vec()).unwrap();
                assert!(text.contains(db::PAIRED_UPGRADES_DISABLED), "{text}");
            }
        }
        assert!(control.upgrade().await.unwrap().is_none());
        let upgrade_id = control
            .queue_paired_upgrade_for_tests(
                &format!("sha256:{}", "a".repeat(64)),
                &format!("sha256:{}", "b".repeat(64)),
                false,
                now,
            )
            .await
            .unwrap()
            .id;
        db.with_conn(|conn| {
            let raw: String = conn.query_row(
                "SELECT value FROM meta WHERE key='component_upgrade_v1'",
                (),
                |row| Ok(row.get(0)?),
            )?;
            let mut value: serde_json::Value = serde_json::from_str(&raw)?;
            value["irreversible"] = serde_json::json!(true);
            let tx = conn.transaction_immediate()?;
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
