//! End-to-end smoke for the dual-process architecture (no Discord network).
//!
//! Mirrors dual-process checks documented in `docs/ARCHITECTURE.md` / CI:
//! 1. Bot + API open the same SQLite file (migrate true / false).
//! 2. API-side write bumps `config_version`; bot poll invalidates cache ≤ poll window.
//! 3. Bot-side write (simulated `!prefix`) is visible via API DB reads.

use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
use jmodguard::db::{self, Db, DbOpenOptions};
use jmodguard_api::web::state::ApiState;
use jmodguard_bot::state::BotState;
use std::path::Path;

fn tmp_db_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard_smoke_{label}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    jmodguard::tenant::remove_sidecars(path);
}

fn open_dual(path: &Path) -> (Db, Db) {
    let bot = Db::open_with(DbOpenOptions {
        read_only: false,
        path: path.to_path_buf(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .expect("bot (migrate=true) must open shared DB");
    let api = Db::open_with(DbOpenOptions {
        read_only: true,
        path: path.to_path_buf(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .expect("api (migrate=false) must open same DB after bot migrated");
    (bot, api)
}

#[tokio::test]
async fn smoke_dual_process_config_sync() {
    let tmp = tmp_db_path("e2e");
    {
        let (bot_db, api_db) = open_dual(&tmp);

        let mut cfg = BotProcessConfig::default();
        cfg.db.path = tmp.clone();
        cfg.bot.config_poll_secs = 1;
        cfg.bot.control_bind = "127.0.0.1:0".into();
        cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        let bot = BotState::new(cfg, bot_db).expect("BotState");
        bot.activation.mark_lock_held(true);
        assert!(
            db::claim_active_bot(
                &bot.db,
                bot.activation.instance(),
                jmodguard::control::now_ms(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await
            .unwrap()
        );
        bot.install_control_actor_lookup(|_actor, _op| {
            Some(db::CachedGuildActor {
                guild_owner_id: 0,
                permissions: 1 << 5,
            })
        });
        let bound = jmodguard_bot::control_server::spawn(std::sync::Arc::clone(&bot))
            .expect("control server")
            .expect("control listener");
        let mut api_cfg = ApiProcessConfig::default();
        api_cfg.db.path = tmp.clone();
        api_cfg.bot.control_target = bound.to_string();
        api_cfg.bot.control_secret = jmodguard::config::ControlSecret::new("x".repeat(32));
        assert!(api_db.is_read_only());
        let api = ApiState::new(api_cfg, api_db).expect("ApiState");

        let guild_id: u64 = 55_555;

        // --- check 1: shared DB, version baseline ----------------------------
        let baseline = db::get_config_version(&bot.db).await.unwrap();
        assert!(baseline > 0, "initial seed must be versioned");
        assert_eq!(db::get_config_version(&api.db).await.unwrap(), baseline);

        // Already primed at state construction — unchanged version must not invalidate.
        assert!(!bot.poll_config_version().await.unwrap());
        assert!(!api.poll_config_version().await.unwrap());

        // Warm bot cache with default prefix.
        let before = bot.configs.get(&bot.db, guild_id).await.unwrap();
        assert_eq!(before.settings.prefix, "!");

        // --- check 2: API save goes through the bot; the API connection cannot write
        let enrollment = db::enroll_guild(&bot.db, guild_id as i64, 1)
            .await
            .unwrap()
            .unwrap();
        let reply = api
            .commit_actor(
                1,
                db::ControlOp::PatchSettings {
                    scope: db::ScopeWire {
                        global: false,
                        guild_id: guild_id as i64,
                        enrollment_id: enrollment.id.clone(),
                        joined_at_ns: 1,
                    },
                    prefix: db::FieldPatch::Set("!from-api".into()),
                    bot_name: db::FieldPatch::Absent,
                    reconcile: false,
                },
            )
            .await
            .unwrap();
        assert!(matches!(reply, db::ControlReply::Saved { .. }));

        assert_eq!(db::get_config_version(&bot.db).await.unwrap(), baseline + 1);
        assert!(
            bot.poll_config_version().await.unwrap(),
            "bot must detect API config_version bump"
        );

        let after_api = bot.configs.get(&bot.db, guild_id).await.unwrap();
        assert_eq!(
            after_api.settings.prefix, "!from-api",
            "bot cache must reload after cross-process write"
        );

        let custom_id = "custom:0123456789abcdef".to_string();
        let custom_rule = jmodguard::types::RuleDef {
            name: "pattern test".into(),
            description: String::new(),
            enabled: true,
            dry_run: true,
            params: serde_json::json!({ "kind": "pattern", "pattern": "scam" }),
            actions: vec![jmodguard::types::ActionDef {
                at: 1,
                action: "delete".into(),
                duration_minutes: 0,
                delete_days: 0,
                reason: "match".into(),
            }],
            whitelist_roles: vec![],
            whitelist_channels: vec![],
            whitelist_users: vec![],
            skip_moderators: false,
            include_channels: vec![],
            log_automod: true,
            open_case: true,
        };
        let version_before_custom = db::get_config_version(&bot.db).await.unwrap();
        let scope = db::ScopeWire {
            global: false,
            guild_id: guild_id as i64,
            enrollment_id: enrollment.id.clone(),
            joined_at_ns: enrollment.joined_at_ns,
        };
        let reply = api
            .commit_actor(
                1,
                db::ControlOp::UpsertRule {
                    request: scope.clone(),
                    target: scope,
                    id: custom_id.clone(),
                    rule: custom_rule,
                },
            )
            .await
            .unwrap();
        assert!(matches!(reply, db::ControlReply::Saved { .. }));
        let stored = db::get_rule(&bot.db, guild_id as i64, &custom_id)
            .await
            .unwrap()
            .expect("bot must persist custom rule");
        assert!(stored.dry_run);
        assert!(
            db::get_config_version(&bot.db).await.unwrap() > version_before_custom,
            "custom rule upsert must bump config_version"
        );
        // --- check 3: bot command write visible to API (guild_config reads DB)
        db::patch_settings(
            &bot.db,
            guild_id as i64,
            Some(Some("!from-bot".into())),
            None,
            None,
        )
        .await
        .unwrap();
        bot.invalidate_configs();

        let api_read = db::get_settings(&api.db, guild_id as i64).await.unwrap();
        assert_eq!(
            api_read.prefix, "!from-bot",
            "API must read bot-side config write from shared DB"
        );

        assert_eq!(db::get_config_version(&api.db).await.unwrap(), baseline + 3);
        assert!(
            api.poll_config_version().await.unwrap(),
            "API must detect bot config_version bump"
        );
    }

    // The DB pools above must be dropped before removing SQLite/WAL/SHM files;
    // Windows cannot unlink an open database file.
    cleanup_db(&tmp);
}

fn service_bin(name: &str) -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test executable");
    path.pop();
    path.pop();
    path.push(format!("{}{}", name, std::env::consts::EXE_SUFFIX));
    path
}

fn wait_status(
    path: &std::path::Path,
    expected: &str,
    child: &mut std::process::Child,
    log: &std::path::Path,
    deadline: std::time::Instant,
) -> String {
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text == expected
        {
            return text;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let logged = std::fs::read_to_string(log).unwrap_or_default();
            panic!("probe exited {status} before {}:\n{logged}", path.display());
        }
        let logged = std::fs::read_to_string(log).unwrap_or_default();
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}:\n{logged}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Requires `jmodguard-bot` and `jmodguard-api` in the same cargo profile
/// directory as this test. `cargo test --workspace` does not build those
/// bins; `scripts/ci-preflight.sh` builds them and passes `--include-ignored`.
#[tokio::test]
#[ignore = "needs the built jmodguard-bot and jmodguard-api binaries"]
async fn smoke_bot_and_api_executables_share_sqlite() {
    use std::process::{Command, Stdio};
    let root = tmp_db_path("os-process").with_extension("");
    std::fs::create_dir(&root).unwrap();
    let database = root.join("app.db");
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[db]\npath={:?}\npool_size=2\n[bot]\nconfig_poll_secs=1\ndefault_prefix='!'\ncontrol_bind='127.0.0.1:0'\n[web]\nbind='127.0.0.1:9'\nbase_url='http://127.0.0.1:9'\noauth_client_id='fixture'\noauth_client_secret='fixture'\n[log]\ndirectory=''\n",
            database.to_string_lossy()
        ),
    )
    .unwrap();
    let bot_bin = service_bin("jmodguard-bot");
    let api_bin = service_bin("jmodguard-api");
    assert!(
        bot_bin.is_file(),
        "build jmodguard-bot before this test: {}",
        bot_bin.display()
    );
    assert!(
        api_bin.is_file(),
        "build jmodguard-api before this test: {}",
        api_bin.display()
    );
    struct Running {
        child: std::process::Child,
        root: std::path::PathBuf,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    let spawn =
        |bin: &std::path::Path, probe: &str, log: &std::path::Path, extra: &[(&str, &str)]| {
            let log_file = std::fs::File::create(log).unwrap();
            let mut cmd = Command::new(bin);
            cmd.env_clear()
                .env("JMODGUARD_CONFIG", &config)
                .env("JMODGUARD_CONTROL_SECRET", "x".repeat(32))
                .env("JMODGUARD_PROCESS_PROBE", probe)
                .env("JMODGUARD_PROBE_DIR", &root)
                .env("JMODGUARD_PROBE_GUILD", "55555")
                .stdout(Stdio::from(log_file.try_clone().unwrap()))
                .stderr(Stdio::from(log_file));
            for (key, value) in extra {
                cmd.env(key, value);
            }
            cmd.spawn().unwrap()
        };
    let bot_log = root.join("bot.log");
    let mut bot = Running {
        child: spawn(
            &bot_bin,
            "bot-cache",
            &bot_log,
            &[("JMODGUARD_PROBE_EXPECT", "!from-api")],
        ),
        root: root.clone(),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    wait_status(
        &root.join("bot-migrate"),
        "true",
        &mut bot.child,
        &bot_log,
        deadline,
    );
    wait_status(
        &root.join("bot-ready"),
        "!",
        &mut bot.child,
        &bot_log,
        deadline,
    );
    let api_log = root.join("api.log");
    let api = spawn(
        &api_bin,
        "api-write",
        &api_log,
        &[("JMODGUARD_PROBE_PREFIX", "!from-api")],
    );
    let mut api = Running { child: api, root };
    wait_status(
        &api.root.join("api-migrate"),
        "false",
        &mut api.child,
        &api_log,
        deadline,
    );
    wait_status(
        &api.root.join("api-wrote"),
        "!from-api",
        &mut api.child,
        &api_log,
        deadline,
    );
    wait_status(
        &api.root.join("bot-refreshed"),
        "!from-api",
        &mut bot.child,
        &bot_log,
        deadline,
    );
    let bot_status = bot.child.wait().unwrap();
    let api_status = api.child.wait().unwrap();
    assert!(bot_status.success(), "bot probe exited {bot_status}");
    assert!(api_status.success(), "api probe exited {api_status}");
}

#[tokio::test]
async fn missing_outage_and_stale_gateway_departures_do_not_invalidate_current_nickname() {
    use jmodguard::db::*;
    use jmodguard_bot::enrollment_repair::observe_gateway_membership;
    let path = tmp_db_path("gateway_nickname");
    let database = Db::open(&path, 2).unwrap();
    let cfg = BotProcessConfig::default();
    let tenant = jmodguard::tenant::TenantRuntime::open(
        &database,
        &cfg.db,
        &cfg.guild_stores,
        &cfg.evidence,
    )
    .unwrap();
    enroll_guild(&database, 10, 101).await.unwrap();
    patch_settings(&database, 10, None, Some(Some("new name".into())), None)
        .await
        .unwrap();
    let job = pending_nickname_reconciles(&database, i64::MAX, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(
        observe_gateway_membership(&database, &tenant, 10, None, false, false)
            .await
            .is_err()
    );
    observe_gateway_membership(&database, &tenant, 10, Some(101), true, false)
        .await
        .unwrap();
    observe_gateway_membership(&database, &tenant, 10, Some(100), false, false)
        .await
        .unwrap();
    assert!(
        nickname_reconcile_can_dispatch(&database, &job)
            .await
            .unwrap()
    );
    observe_gateway_membership(&database, &tenant, 10, Some(101), false, false)
        .await
        .unwrap();
    assert!(
        !nickname_reconcile_can_dispatch(&database, &job)
            .await
            .unwrap()
    );
    drop(database);
    cleanup_db(&path);
}
