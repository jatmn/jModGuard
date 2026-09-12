//! Authenticated HTTP regressions with local OAuth responses and real SQLite.
use super::*;
use crate::db;
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tower::ServiceExt;

struct DiscordStub {
    guilds: std::sync::Mutex<Vec<oauth::DiscordGuild>>,
    failed: AtomicBool,
    calls: AtomicUsize,
    retire_during_lookup: std::sync::Mutex<Option<db::Db>>,
    retire_destination_on_second_lookup: std::sync::Mutex<Option<db::Db>>,
}
struct Fixture {
    app: Arc<ApiState>,
    router: Router,
    cookie: String,
    discord: Arc<DiscordStub>,
    server: tokio::task::JoinHandle<()>,
    path: std::path::PathBuf,
}
fn managed_guild(id: u64) -> oauth::DiscordGuild {
    oauth::DiscordGuild {
        id: id.to_string(),
        name: "fixture".into(),
        icon: None,
        owner: false,
        permissions: Some(oauth::PermissionsField::Number((1 << 5) | (1 << 2))),
    }
}
impl Fixture {
    async fn new(operator: bool) -> Self {
        let discord = Arc::new(DiscordStub {
            guilds: std::sync::Mutex::new(vec![managed_guild(10), managed_guild(20)]),
            failed: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            retire_during_lookup: std::sync::Mutex::new(None),
            retire_destination_on_second_lookup: std::sync::Mutex::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::clone(&discord);
        let routes = Router::new().route(
            "/guilds",
            get(move |headers: HeaderMap| {
                let handler = Arc::clone(&handler);
                async move {
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer fixture-token"
                    );
                    let call = handler.calls.fetch_add(1, Ordering::SeqCst);
                    if call == 1 {
                        let db = handler
                            .retire_destination_on_second_lookup
                            .lock()
                            .unwrap()
                            .take();
                        if let Some(db) = db {
                            db::enroll_guild(&db, 20, 101).await.unwrap();
                        }
                    }
                    let retire = handler.retire_during_lookup.lock().unwrap().take();
                    if let Some(db) = retire {
                        db::retire_guild_enrollment(&db, 10, 100).await.unwrap();
                    }
                    if handler.failed.load(Ordering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"error":"offline"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!(*handler.guilds.lock().unwrap())),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let path = std::env::temp_dir().join(format!(
            "jmodguard_api_enrollment_{:032x}.db",
            rand::random::<u128>()
        ));
        let db = db::Db::open(&path, 2).unwrap();
        let mut cfg = crate::config::ApiProcessConfig::default();
        cfg.bot.owner_id = if operator { 42 } else { 99 };
        let mut app = ApiState::new(cfg, db).unwrap();
        Arc::get_mut(&mut app).unwrap().oauth.guilds_endpoint =
            Some(format!("http://{address}/guilds"));
        tests::seed_auth_caches(&app, 42, vec![managed_guild(10), managed_guild(20)]);
        let store = MemoryStore::default();
        let session = Session::new(None, Arc::new(store.clone()), None);
        session.insert(SESSION_USER, 42u64).await.unwrap();
        session
            .insert(SESSION_TOKEN, "fixture-token")
            .await
            .unwrap();
        session.save().await.unwrap();
        let cookie = format!("{SESSION_COOKIE_NAME}={}", session.id().unwrap());
        let router = router_with_store(Arc::clone(&app), store);
        Self {
            app,
            router,
            cookie,
            discord,
            server,
            path,
        }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let guild = path
            .split('/')
            .nth(3)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0);
        let token = if guild > 0 {
            db::guild_enrollment(&self.app.db, guild)
                .await
                .ok()
                .flatten()
                .map(|e| e.id)
        } else {
            None
        };
        let mut value: serde_json::Value =
            serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        if path.ends_with("/copy") {
            let source = value["from_guild_id"].as_i64().unwrap_or(0);
            value["from_enrollment_id"] = serde_json::json!(
                db::guild_enrollment(&self.app.db, source)
                    .await
                    .ok()
                    .flatten()
                    .map(|e| e.id)
            );
        }
        let owned = if value.is_null() {
            body.to_owned()
        } else {
            value.to_string()
        };
        self.request_token(method, path, &owned, token.as_deref())
            .await
    }
    async fn request_token(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", &self.cookie)
                    .header("content-type", "application/json")
                    .header("x-jmodguard-enrollment", token.unwrap_or(""))
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8(bytes.to_vec()).unwrap())
            }),
        )
    }
    async fn enroll(&self, guild: i64) {
        db::enroll_guild(&self.app.db, guild, 100).await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        crate::tenant::remove_sidecars(&self.path);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

#[tokio::test]
async fn retired_and_unregistered_guild_routes_deny_even_installation_owner() {
    for operator in [false, true] {
        let f = Fixture::new(operator).await;
        f.enroll(10).await;
        db::patch_settings(&f.app.db, 10, Some(Some("private".into())), None, None)
            .await
            .unwrap();
        db::retire_guild_enrollment(&f.app.db, 10, 100)
            .await
            .unwrap();
        for guild in [10, 20] {
            for suffix in [
                "config",
                "rules/anything",
                "badwords",
                "commands",
                "commands/anything",
                "templates",
                "templates/message_edit",
                "logchannels",
                "logchannels/message_edit",
                "events",
                "bans",
                "stats",
                "cases",
                "case-delete-roles",
            ] {
                let (status, body) = f
                    .request("GET", &format!("/api/guilds/{guild}/{suffix}"), "")
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "{suffix}: {body}");
                assert_eq!(body["code"], "guild_enrollment_inactive");
                assert!(!body.to_string().contains("private"));
            }
        }
        for (method, suffix, body) in [
            (
                "PUT",
                "settings",
                r#"{"prefix":"changed","bot_name":"changed"}"#,
            ),
            ("DELETE", "badwords/1", ""),
            ("DELETE", "commands/anything", r#"{}"#),
            ("DELETE", "rules/anything", r#"{}"#),
            ("DELETE", "templates/message_edit", r#"{}"#),
            (
                "DELETE",
                "logchannels/message_edit",
                r#"{"request_id":"1000_clear"}"#,
            ),
            ("POST", "testregex", r#"{"pattern":"a","sample":"a"}"#),
            (
                "POST",
                "unban",
                r#"{"request_id":"1000_unban","user_id":"99"}"#,
            ),
            ("DELETE", "cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", ""),
            (
                "PUT",
                "case-delete-roles",
                r#"{"roles":[],"revision":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
            ),
        ] {
            let (status, value) = f
                .request(method, &format!("/api/guilds/10/{suffix}"), body)
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}: {value}");
            assert_eq!(value["code"], "guild_enrollment_inactive");
        }
        assert_eq!(
            db::get_settings(&f.app.db, 10).await.unwrap().prefix,
            "private"
        );
        assert!(
            db::nickname_reconcile_status(&f.app.db, 10)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn current_enrollment_allows_access_but_live_permission_revocation_wins_over_cache() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let (status, _) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"current"}"#)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
    f.discord.guilds.lock().unwrap().clear();
    let (status, body) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"revoked"}"#)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("cannot manage"));
    assert_eq!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "current"
    );
    assert_eq!(f.discord.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn copy_requires_both_enrollments_even_with_global_destination() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    db::patch_settings(&f.app.db, 10, Some(Some("source".into())), None, None)
        .await
        .unwrap();
    assert_eq!(
        f.request(
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#
        )
        .await
        .0,
        StatusCode::OK
    );
    db::retire_guild_enrollment(&f.app.db, 10, 100)
        .await
        .unwrap();
    for destination in [0, 20] {
        let (status, body) = f
            .request(
                "POST",
                &format!("/api/guilds/{destination}/copy"),
                r#"{"from_guild_id":10,"sections":["settings"]}"#,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
    let (status, _) = f
        .request(
            "POST",
            "/api/guilds/10/copy",
            r#"{"from_guild_id":20,"sections":["settings"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_ne!(
        db::get_settings(&f.app.db, 0).await.unwrap().prefix,
        "source"
    );
}

#[tokio::test]
async fn unavailable_membership_database_fails_closed_without_leaking_details() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch("ALTER TABLE guild_enrollments RENAME TO private_fixture_table")
        })
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "guild_access_unverified");
    assert!(!body.to_string().contains("table"));
    assert_eq!(
        f.request("GET", "/api/guilds/0/config", "").await.0,
        StatusCode::OK,
        "global owner scope needs no guild enrollment"
    );
    f.app
        .db
        .with_conn(|conn| {
            conn.execute_batch("ALTER TABLE private_fixture_table RENAME TO guild_enrollments")
        })
        .await
        .unwrap();
    assert_eq!(
        f.request("GET", "/api/guilds/10/config", "").await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn discord_failure_never_uses_cached_authority_or_retires_membership() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.discord.failed.store(true, Ordering::SeqCst);
    let (status, _) = f.request("GET", "/api/guilds/10/config", "").await;
    assert!(!status.is_success());
    assert!(db::guild_enrollment(&f.app.db, 10).await.unwrap().is_some());
}

#[tokio::test]
async fn retirement_during_discord_authorization_blocks_the_settings_write() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    *f.discord.retire_during_lookup.lock().unwrap() = Some(f.app.db.clone());
    let (status, body) = f
        .request("PUT", "/api/guilds/10/settings", r#"{"prefix":"too late"}"#)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_ne!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "too late"
    );
}

#[tokio::test]
async fn out_of_range_guild_ids_cannot_wrap_into_database_scopes() {
    let f = Fixture::new(true).await;
    f.discord
        .guilds
        .lock()
        .unwrap()
        .push(managed_guild(u64::MAX));
    let (status, body) = f
        .request(
            "PUT",
            &format!("/api/guilds/{}/settings", u64::MAX),
            r#"{"prefix":"overflow"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid guild id");
    assert_ne!(
        db::get_settings(&f.app.db, -1).await.unwrap().prefix,
        "overflow"
    );
}

#[tokio::test]
async fn copy_cannot_write_destination_replaced_after_its_authorization() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    db::patch_settings(&f.app.db, 10, Some(Some("source".into())), None, None)
        .await
        .unwrap();
    *f.discord
        .retire_destination_on_second_lookup
        .lock()
        .unwrap() = Some(f.app.db.clone());
    let (status, body) = f
        .request(
            "POST",
            "/api/guilds/20/copy",
            r#"{"from_guild_id":10,"sections":["settings"]}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "guild_enrollment_changed");
    assert_ne!(
        db::get_settings(&f.app.db, 20).await.unwrap().prefix,
        "source"
    );
}

#[tokio::test]
async fn configuration_crud_routes_use_scoped_writes_for_current_guild_and_global() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let rule = crate::defaults::default_rules().remove(0);
    let mut command = crate::defaults::default_commands().remove(0);
    command.name = "fixture".into();
    for (guild, scope) in [(10, "guild"), (10, "global"), (0, "global")] {
        let base = format!("/api/guilds/{guild}");
        let rule_path = format!("{base}/rules/{}", crate::types::RULE_IDS[0]);
        for (method, path, body) in [
            (
                "PUT",
                rule_path.clone(),
                serde_json::json!({"scope":scope,"data":rule}).to_string(),
            ),
            (
                "POST",
                format!("{base}/commands"),
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                format!("{base}/commands/fixture"),
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                format!("{base}/templates/message_edit"),
                serde_json::json!({"scope":scope,"template":"fixture"}).to_string(),
            ),
        ] {
            let (status, body) = f.request(method, &path, &body).await;
            assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        }
        let (status, body) = f
            .request(
                "POST",
                &format!("{base}/badwords"),
                &serde_json::json!({"scope":scope,"pattern":"fixture"}).to_string(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["id"].as_i64().unwrap();
        for path in [
            rule_path,
            format!("{base}/commands/fixture"),
            format!("{base}/templates/message_edit"),
            format!("{base}/badwords/{id}"),
        ] {
            let (status, body) = f
                .request(
                    "DELETE",
                    &path,
                    &serde_json::json!({"scope":scope}).to_string(),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
        }
    }
}

#[tokio::test]
async fn guild_manager_cannot_mutate_global_configuration_or_probe_foreign_badwords() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let global = db::add_badword(&f.app.db, 0, "global", None).await.unwrap();
    let other = db::add_badword(&f.app.db, 20, "other", None).await.unwrap();
    for id in [global, other, i64::MAX] {
        let (status, body) = f
            .request("DELETE", &format!("/api/guilds/10/badwords/{id}"), "")
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "badword not found");
    }
    let (status, _) = f
        .request(
            "PUT",
            "/api/guilds/10/templates/message_edit",
            r#"{"scope":"global","template":"rejected"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        db::get_badword_primary(&f.app.db, global)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db::get_badword_primary(&f.app.db, other)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn configuration_edits_reject_missing_and_replaced_page_memberships() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (status, loaded) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_eq!(status, StatusCode::OK);
    let old = loaded["enrollment_id"].as_str().unwrap();
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let rule = crate::defaults::default_rules().remove(0);
    let command = crate::defaults::default_commands().remove(0);
    let version = db::get_config_version(&f.app.db).await.unwrap();
    for scope in ["guild", "global"] {
        let scope_body = serde_json::json!({"scope":scope}).to_string();
        for (method, suffix, body) in [
            ("PUT", "settings", r#"{"prefix":"stale"}"#.to_owned()),
            (
                "PUT",
                "rules/flood",
                serde_json::json!({"scope":scope,"data":rule}).to_string(),
            ),
            ("DELETE", "rules/flood", scope_body.clone()),
            (
                "POST",
                "badwords",
                serde_json::json!({"scope":scope,"pattern":"stale"}).to_string(),
            ),
            ("DELETE", "badwords/1", String::new()),
            (
                "POST",
                "commands",
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            (
                "PUT",
                "commands/fixture",
                serde_json::json!({"scope":scope,"data":command}).to_string(),
            ),
            ("DELETE", "commands/fixture", scope_body.clone()),
            (
                "PUT",
                "templates/message_edit",
                serde_json::json!({"scope":scope,"template":"stale"}).to_string(),
            ),
            ("DELETE", "templates/message_edit", scope_body),
            (
                "POST",
                "copy",
                r#"{"from_guild_id":"20","sections":["settings"]}"#.to_owned(),
            ),
        ] {
            for token in [None, Some(old)] {
                let (status, body) = f
                    .request_token(method, &format!("/api/guilds/10/{suffix}"), &body, token)
                    .await;
                assert_eq!(status, StatusCode::CONFLICT, "{method} {suffix}: {body}");
                assert_eq!(body["code"], "guild_enrollment_changed");
            }
        }
    }
    assert_eq!(db::get_config_version(&f.app.db).await.unwrap(), version);
    let (_, fresh) = f.request("GET", "/api/guilds/10/config", "").await;
    assert_ne!(fresh["enrollment_id"], loaded["enrollment_id"]);
    let (status, body) = f
        .request_token(
            "PUT",
            "/api/guilds/10/settings",
            r#"{"prefix":"fresh"}"#,
            fresh["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        db::get_settings(&f.app.db, 10).await.unwrap().prefix,
        "fresh"
    );
}

#[tokio::test]
async fn copy_preserves_the_source_selected_by_the_browser() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let (_, source) = f.request("GET", "/api/guilds/10/config", "").await;
    let (_, dest) = f.request("GET", "/api/guilds/20/config", "").await;
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let version = db::get_config_version(&f.app.db).await.unwrap();
    for guild in [0, 20] {
        for token in [serde_json::Value::Null, source["enrollment_id"].clone()] {
            let body = serde_json::json!({"from_guild_id":"10", "from_enrollment_id":token, "sections":["settings"]});
            let (status, body) = f
                .request_token(
                    "POST",
                    &format!("/api/guilds/{guild}/copy"),
                    &body.to_string(),
                    dest["enrollment_id"].as_str(),
                )
                .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], "guild_enrollment_changed");
        }
    }
    assert_eq!(db::get_config_version(&f.app.db).await.unwrap(), version);
    let (_, source) = f.request("GET", "/api/guilds/10/config", "").await;
    let body = serde_json::json!({"from_guild_id":"10", "from_enrollment_id":source["enrollment_id"], "sections":["settings"]});
    let (status, body) = f
        .request_token(
            "POST",
            "/api/guilds/20/copy",
            &body.to_string(),
            dest["enrollment_id"].as_str(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn configuration_snapshot_checks_its_captured_membership() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let old = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    let result =
        db::load_enrolled_web_config_snapshot(&f.app.db, db::ConfigurationScope::Guild(old)).await;
    assert!(result.err().unwrap().is::<db::EnrollmentChanged>());
    let current = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::load_enrolled_web_config_snapshot(&f.app.db, db::ConfigurationScope::Guild(current))
        .await
        .unwrap();
}

fn action_key(label: &str) -> String {
    format!("{}_{}", chrono::Utc::now().timestamp(), label)
}

#[tokio::test]
async fn queued_log_change_is_applied_only_by_worker_and_receipt_is_replayable() {
    use jmodguard_bot::action_worker::{DiscordActions, Outcome, WorkerLimits};
    struct Allowed;
    #[async_trait::async_trait]
    impl DiscordActions for Allowed {
        async fn membership_matches(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn authorized(&self, _: &db::BotAction) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn valid_log_channel(&self, _: i64, _: i64) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn unban(&self, _: i64, _: i64, _: &str) -> Outcome {
            panic!("not an unban")
        }
    }
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let body = serde_json::json!({"request_id":action_key("set"),"channel_id":"123"}).to_string();
    let (status, queued) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
    assert!(queued.get("claim").is_none());
    assert!(
        db::list_log_channels(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, replay) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    jmodguard_bot::action_worker::drain_with_tenant(
        &f.app.db,
        &f.app.tenant,
        &Allowed,
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    let (status, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["state"], "succeeded");
    assert_eq!(
        db::list_log_channels(&f.app.db, 10).await.unwrap(),
        vec![("message_edit".into(), 123)]
    );
    let (_, replay) = f
        .request("PUT", "/api/guilds/10/logchannels/message_edit", &body)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    assert_eq!(replay["state"], "succeeded");

    let clear = serde_json::json!({"request_id":action_key("clear")}).to_string();
    let (status, queued) = f
        .request("DELETE", "/api/guilds/10/logchannels/message_edit", &clear)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "pending");
    assert_eq!(db::list_log_channels(&f.app.db, 10).await.unwrap().len(), 1);
    jmodguard_bot::action_worker::drain_with_tenant(
        &f.app.db,
        &f.app.tenant,
        &Allowed,
        &WorkerLimits::default(),
    )
    .await
    .unwrap();
    assert!(
        db::list_log_channels(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let (_, replay) = f
        .request("DELETE", "/api/guilds/10/logchannels/message_edit", &clear)
        .await;
    assert_eq!(replay["id"], queued["id"]);
    assert_eq!(replay["state"], "succeeded");
}

#[tokio::test]
async fn actions_require_page_membership_and_reject_rebinding_saved_requests() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let (_, config) = f.request("GET", "/api/guilds/10/config", "").await;
    let old = config["enrollment_id"].as_str();
    let body = serde_json::json!({"request_id":action_key("unban"),"user_id":"99"}).to_string();
    let (_, queued) = f.request("POST", "/api/guilds/10/unban", &body).await;
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for token in [None, old] {
        for (method, path, body) in [
            ("POST", "/api/guilds/10/unban".to_owned(), body.clone()),
            (
                "PUT",
                "/api/guilds/10/logchannels/message_edit".into(),
                serde_json::json!({"request_id":action_key("log"),"channel_id":"123"}).to_string(),
            ),
            (
                "DELETE",
                "/api/guilds/10/logchannels/message_edit".into(),
                serde_json::json!({"request_id":action_key("clear")}).to_string(),
            ),
            ("GET", "/api/guilds/10/actions".into(), String::new()),
            (
                "DELETE",
                format!("/api/guilds/10/actions/{}", queued["id"].as_str().unwrap()),
                String::new(),
            ),
        ] {
            let (status, body) = f.request_token(method, &path, &body, token).await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {path}: {body}");
        }
    }
    let (status, rejected) = f.request("POST", "/api/guilds/10/unban", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{rejected}");
    assert_eq!(rejected["code"], "action_enrollment_changed");
    assert!(
        db::recent_bot_actions(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn action_visibility_cancellation_and_admission_follow_current_action_permissions() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let (_, unban) = f
        .request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("ban"),"user_id":"99"}).to_string(),
        )
        .await;
    let (_, log) = f
        .request(
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            &serde_json::json!({"request_id":action_key("log"),"channel_id":"123"}).to_string(),
        )
        .await;
    let mut manage_only = managed_guild(10);
    manage_only.permissions = Some(oauth::PermissionsField::Number(1 << 5));
    *f.discord.guilds.lock().unwrap() = vec![manage_only];
    let (status, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["id"], log["id"]);
    let path = format!("/api/guilds/10/actions/{}", unban["id"].as_str().unwrap());
    assert_eq!(
        f.request("DELETE", &path, "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            "POST",
            "/api/guilds/10/unban",
            &serde_json::json!({"request_id":action_key("denied"),"user_id":"100"}).to_string()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let path = format!("/api/guilds/10/actions/{}", log["id"].as_str().unwrap());
    assert_eq!(f.request("DELETE", &path, "").await.0, StatusCode::OK);
    let mut ban_only = managed_guild(10);
    ban_only.permissions = Some(oauth::PermissionsField::Number(1 << 2));
    *f.discord.guilds.lock().unwrap() = vec![ban_only];
    let (_, history) = f.request("GET", "/api/guilds/10/actions", "").await;
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["id"], unban["id"]);
    assert_eq!(
        f.request("DELETE", &path, "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request("GET", "/api/guilds/20/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request("GET", "/api/guilds/0/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
    f.discord.guilds.lock().unwrap().clear();
    assert_eq!(
        f.request("GET", "/api/guilds/10/actions", "").await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn action_requests_require_ids_reject_forged_authority_and_report_expiry() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for (method, path, body) in [
        (
            "POST",
            "/api/guilds/10/unban",
            serde_json::json!({"user_id":"99"}),
        ),
        (
            "PUT",
            "/api/guilds/10/logchannels/message_edit",
            serde_json::json!({"channel_id":"123"}),
        ),
        (
            "DELETE",
            "/api/guilds/10/logchannels/message_edit",
            serde_json::json!({}),
        ),
        (
            "POST",
            "/api/guilds/10/unban",
            serde_json::json!({"request_id":action_key("forged"),"user_id":"99","actor_id":"123"}),
        ),
    ] {
        assert_eq!(
            f.request(method, path, &body.to_string()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for (key, status, code) in [
        (
            "invalid".to_owned(),
            StatusCode::BAD_REQUEST,
            "action_request_invalid",
        ),
        (
            format!("{}_old", chrono::Utc::now().timestamp() - 10 * 86400),
            StatusCode::GONE,
            "action_request_expired",
        ),
    ] {
        let (actual, body) = f
            .request(
                "POST",
                "/api/guilds/10/unban",
                &serde_json::json!({"request_id":key,"user_id":"99"}).to_string(),
            )
            .await;
        assert_eq!(actual, status, "{body}");
        assert_eq!(body["code"], code);
    }
    assert!(
        db::recent_bot_actions(&f.app.db, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn moderation_pages_require_loaded_membership_and_current_permissions() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let old = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    db::insert_event(
        &f.app.db,
        10,
        "member_ban",
        Some(123),
        None,
        &serde_json::json!({"reason":"fixture"}),
    )
    .await
    .unwrap();
    db::record_ban(&f.app.db, 10, 123, None, "fixture")
        .await
        .unwrap();
    for path in ["events?user=123&limit=1", "bans?limit=1", "stats"] {
        let path = format!("/api/guilds/10/{path}");
        let (status, body) = f.request_token("GET", &path, "", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(
            f.request_token("GET", &path, "", Some(&old.id)).await.0,
            StatusCode::OK
        );
    }
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for route in ["events", "bans", "stats"] {
        let path = format!("/api/guilds/10/{route}");
        let (status, body) = f.request_token("GET", &path, "", Some(&old.id)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(f.request("GET", &path, "").await.0, StatusCode::OK);
        assert_eq!(
            f.request("GET", &format!("/api/guilds/0/{route}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    f.discord.guilds.lock().unwrap().clear();
    for route in ["events", "bans", "stats"] {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{route}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn moderation_event_user_filter_rejects_ids_outside_database_range() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let (status, body) = f
        .request("GET", "/api/guilds/10/events?user=18446744073709551615", "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid user id");
}

fn configuration_read_routes() -> Vec<String> {
    vec![
        format!("rules/{}", crate::types::RULE_IDS[0]),
        "badwords".into(),
        "commands".into(),
        "commands/fixture".into(),
        "templates".into(),
        "templates/fixture".into(),
        "logchannels".into(),
        "logchannels/fixture".into(),
    ]
}

#[tokio::test]
async fn individual_configuration_reads_require_page_membership_and_fresh_guild_permission() {
    let f = Fixture::new(true).await;
    f.enroll(10).await;
    let original = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    for suffix in configuration_read_routes() {
        let path = format!("/api/guilds/10/{suffix}");
        let (status, body) = f.request_token("GET", &path, "", None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(
            f.request_token("GET", &path, "", Some(&original.id))
                .await
                .0,
            StatusCode::OK
        );
    }
    db::enroll_guild(&f.app.db, 10, 101).await.unwrap();
    for suffix in configuration_read_routes() {
        let path = format!("/api/guilds/10/{suffix}");
        let (status, body) = f.request_token("GET", &path, "", Some(&original.id)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "guild_enrollment_changed");
        assert_eq!(f.request("GET", &path, "").await.0, StatusCode::OK);
    }
    f.discord.guilds.lock().unwrap().clear();
    for suffix in configuration_read_routes() {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{suffix}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        // Explicit owner global context remains independent of any guild membership.
        assert_eq!(
            f.request_token("GET", &format!("/api/guilds/0/{suffix}"), "", None)
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn individual_configuration_reads_keep_global_context_owner_only() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for suffix in configuration_read_routes() {
        assert_eq!(
            f.request("GET", &format!("/api/guilds/0/{suffix}"), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            f.request("GET", &format!("/api/guilds/10/{suffix}"), "")
                .await
                .0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn individual_configuration_reads_preserve_inheritance_and_log_channel_wire_shapes() {
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    for (guild, label) in [(0, "global"), (10, "guild"), (20, "foreign")] {
        db::upsert_template(&f.app.db, guild, "fixture", label)
            .await
            .unwrap();
        let mut command = crate::defaults::default_commands()[0].clone();
        command.name = "fixture".into();
        command.description = label.into();
        db::upsert_command(&f.app.db, guild, "fixture", &command)
            .await
            .unwrap();
        db::upsert_log_channel(&f.app.db, guild, "fixture", 1329331947876479006 + guild)
            .await
            .unwrap();
        db::add_badword(&f.app.db, guild, label, None)
            .await
            .unwrap();
    }
    let (_, template) = f
        .request("GET", "/api/guilds/10/templates/fixture", "")
        .await;
    assert_eq!(
        template,
        serde_json::json!({"effective":"guild","guild_override":"guild","global":"global"})
    );
    let (_, command) = f
        .request("GET", "/api/guilds/10/commands/FIXTURE", "")
        .await;
    assert_eq!(command["effective"]["description"], "guild");
    assert_eq!(command["global"]["description"], "global");
    db::delete_template(&f.app.db, 10, "fixture").await.unwrap();
    let (_, inherited) = f
        .request("GET", "/api/guilds/10/templates/fixture", "")
        .await;
    assert_eq!(inherited["effective"], "global");
    assert!(inherited["guild_override"].is_null());
    let (_, channels) = f.request("GET", "/api/guilds/10/logchannels", "").await;
    assert_eq!(
        channels,
        serde_json::json!({"guild":[{"event":"fixture","channel_id":"1329331947876479016"}],"global":[]})
    );
    let (_, channel) = f
        .request("GET", "/api/guilds/10/logchannels/fixture", "")
        .await;
    assert_eq!(
        channel,
        serde_json::json!({"effective":"1329331947876479016","guild_override":"1329331947876479016","global":null})
    );
    let (_, absent) = f
        .request("GET", "/api/guilds/10/logchannels/absent", "")
        .await;
    assert_eq!(
        absent,
        serde_json::json!({"effective":null,"guild_override":null,"global":null})
    );
    let (_, words) = f.request("GET", "/api/guilds/10/badwords", "").await;
    let patterns: Vec<_> = words
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["pattern"].as_str().unwrap())
        .collect();
    assert!(
        patterns.contains(&"guild")
            && patterns.contains(&"global")
            && !patterns.contains(&"foreign")
    );
}

#[tokio::test]
async fn cases_are_isolated_and_deletion_requires_administrator() {
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource,
    };
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    f.enroll(20).await;
    let left = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    let right = db::guild_enrollment(&f.app.db, 20).await.unwrap().unwrap();
    f.app.tenant.activate(left.clone()).await.unwrap().unwrap();
    f.app.tenant.activate(right.clone()).await.unwrap().unwrap();
    f.app
        .tenant
        .record_selected_case(
            left.clone(),
            CaseData {
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Warning,
                outcome: CaseOutcome::Succeeded,
                subject_id: 1,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("secret".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids = body["cases"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(body["can_delete"], false);
    let (status, other) = f.request("GET", "/api/guilds/20/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert!(other["cases"].as_array().unwrap().is_empty());
    let id = ids[0].as_str().unwrap();
    let (status, leaked) = f
        .request("GET", &format!("/api/guilds/20/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{leaked}");
    assert!(!leaked.to_string().contains("secret"));
    let (status, denied) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{denied}");
}

#[tokio::test]
async fn administrator_can_delete_cases_and_offline_stores_fail_closed() {
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource,
    };
    use crate::db::guild_stores::{StoreId, StorePolicy, assign_initial_placement, register_store};
    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    f.app
        .tenant
        .record_selected_case(
            enrollment.clone(),
            CaseData {
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Kick,
                outcome: CaseOutcome::Succeeded,
                subject_id: 2,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("private-reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["cases"][0].as_str().unwrap().to_owned();
    f.discord.guilds.lock().unwrap()[0].owner = true;
    f.app.guild_cache.clear();
    let (status, deleted) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["removed"], true);
    assert_eq!(deleted["cleanup_pending"], false);
    assert_eq!(deleted["discord_action_reversed"], false);
    let (status, missing) = f
        .request("GET", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");
    assert!(!missing.to_string().contains("private-reason"));

    let ghost = Fixture::new(false).await;
    ghost.enroll(10).await;
    let store = StoreId::new();
    register_store(&ghost.app.db, store.clone(), StorePolicy::Shared)
        .await
        .unwrap();
    assign_initial_placement(&ghost.app.db, 10, store)
        .await
        .unwrap();
    let (status, offline) = ghost.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{offline}");
    assert_eq!(offline["code"], "guild_store_offline");
    assert!(!offline.to_string().contains("private-reason"));

    let (status, stats) = ghost.request("GET", "/api/guilds/10/stats", "").await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["events"], 0);
    assert_eq!(stats["warns"], 0);
    assert_eq!(stats["bans"], 0);
    assert!(
        stats.get("cases").is_none(),
        "offline tenant stats omit only their unavailable case count: {stats}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn case_delete_reports_cleanup_pending_when_object_cleanup_fails() {
    use crate::db::SqlExecutor;
    use crate::db::guild_data::{
        CaseAction, CaseData, CaseEvidence, CaseId, CaseOutcome, CaseSource, EvidenceCapture,
        EvidenceOrigin, EvidenceRequest,
    };
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let f = Fixture::new(false).await;
    f.enroll(10).await;
    let enrollment = db::guild_enrollment(&f.app.db, 10).await.unwrap().unwrap();
    f.app
        .tenant
        .activate(enrollment.clone())
        .await
        .unwrap()
        .unwrap();
    f.app
        .tenant
        .record_selected_case(
            enrollment.clone(),
            CaseData {
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Kick,
                outcome: CaseOutcome::Succeeded,
                subject_id: 2,
                actor_id: None,
                occurred_at_ms: 1,
                reason: Some("private-reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
        )
        .await
        .unwrap();
    let (status, body) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["cases"][0].as_str().unwrap().to_owned();
    let case = CaseId::try_from(id.clone()).unwrap();
    let (store, scope) = f.app.tenant.live(enrollment.clone()).await.unwrap();
    let storage = CaseId::try_from(db::evidence_storage_id(&f.app.db, "").await.unwrap()).unwrap();
    let evidence_id = CaseId::new();
    store
        .reserve_case_evidence(
            scope.clone(),
            case.clone(),
            evidence_id.clone(),
            EvidenceRequest {
                storage_id: storage,
                bytes: 3,
                origin: EvidenceOrigin {
                    channel_id: 11,
                    message_id: 12,
                    attachment_id: 13,
                },
            },
            crate::tenant::evidence_limits(),
        )
        .await
        .unwrap();
    store
        .settle_evidence_capture(
            scope,
            case.clone(),
            evidence_id.clone(),
            EvidenceCapture::Stored {
                bytes: 3,
                sha256: "a".repeat(64),
            },
        )
        .await
        .unwrap();

    let mut root = f.path.as_os_str().to_owned();
    root.push(".evidence");
    let object = std::path::PathBuf::from(root)
        .join("evidence")
        .join("10")
        .join(&enrollment.id)
        .join(case.as_str())
        .join(format!("{}.data", evidence_id.as_str()));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(object.parent().unwrap())
        .unwrap();
    std::fs::write(&object, b"bad").unwrap();
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o644)).unwrap();

    f.discord.guilds.lock().unwrap()[0].owner = true;
    f.app.guild_cache.clear();
    let (status, deleted) = f
        .request("DELETE", &format!("/api/guilds/10/cases/{id}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["removed"], true);
    assert_eq!(deleted["cleanup_pending"], true);
    assert_eq!(deleted["discord_action_reversed"], false);

    let (status, listed) = f.request("GET", "/api/guilds/10/cases", "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["cases"].as_array().unwrap().is_empty());
    let intents: i64 = f
        .app
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM backup_cleanup_intents WHERE kind='case'",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(intents, 1);
}
