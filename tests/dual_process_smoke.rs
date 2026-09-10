#![cfg(feature = "web")]

//! End-to-end smoke for the dual-process architecture (no Discord network).
//!
//! Mirrors dual-process checks documented in `docs/ARCHITECTURE.md` / CI:
//! 1. Bot + API open the same SQLite file (migrate true / false).
//! 2. API-side write bumps `config_version`; bot poll invalidates cache ≤ poll window.
//! 3. Bot-side write (simulated `!prefix`) is visible via API DB reads.

use jmodguard::config::{ApiProcessConfig, BotProcessConfig};
use jmodguard::db::{self, Db, DbOpenOptions};
use jmodguard::state::BotState;
use jmodguard::web::state::ApiState;
use std::path::PathBuf;

fn tmp_db_path(label: &str) -> PathBuf {
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
}

fn open_dual(path: &PathBuf) -> (Db, Db) {
    let bot = Db::open_with(DbOpenOptions {
        path: path.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .expect("bot (migrate=true) must open shared DB");
    let api = Db::open_with(DbOpenOptions {
        path: path.clone(),
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
        cfg.bot.config_poll_secs = 1;
        let bot = BotState::new(cfg, bot_db).expect("BotState");
        let api = ApiState::new(ApiProcessConfig::default(), api_db).expect("ApiState");

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

        // --- check 2: API write → version bump → bot cache refresh ------------
        db::patch_settings(
            &api.db,
            guild_id as i64,
            Some(Some("!from-api".into())),
            None,
            None,
        )
        .await
        .unwrap();

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

        assert_eq!(db::get_config_version(&api.db).await.unwrap(), baseline + 2);
        assert!(
            api.poll_config_version().await.unwrap(),
            "API must detect bot config_version bump"
        );
    }

    // The DB pools above must be dropped before removing SQLite/WAL/SHM files;
    // Windows cannot unlink an open database file.
    cleanup_db(&tmp);
}

#[tokio::test]
async fn api_process_starts_without_a_discord_bot_token() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let root = tmp_db_path("tokenless_api").with_extension("");
    std::fs::create_dir(&root).unwrap();
    let database = root.join("app.db");
    drop(Db::open(&database, 2).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, format!(
        "[db]\npath={:?}\npool_size=2\n[web]\nbind={:?}\nbase_url={:?}\noauth_client_id='fixture'\noauth_client_secret='fixture'\n[log]\ndirectory=''\n",
        database.to_string_lossy(), address.to_string(), format!("http://{address}")
    )).unwrap();
    drop(listener);
    struct RunningApi {
        child: std::process::Child,
        root: PathBuf,
    }
    impl Drop for RunningApi {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    let mut process = RunningApi {
        child: Command::new(env!("CARGO_BIN_EXE_jmodguard-api"))
            .env_clear()
            .env("JMODGUARD_CONFIG", &config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        root,
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "API exited before serving without a bot token"
        );
        if let Ok(response) = client.get(format!("http://{address}/api/me")).send().await
            && response.status() == reqwest::StatusCode::BAD_REQUEST
        {
            let body = response.text().await.unwrap();
            assert!(body.contains("not authenticated"), "{body}");
            break;
        }
        assert!(Instant::now() < deadline, "API did not finish startup");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
