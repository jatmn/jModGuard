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
async fn missing_outage_and_stale_gateway_departures_do_not_invalidate_current_nickname() {
    use jmodguard::db::*;
    use jmodguard_bot::enrollment_repair::observe_gateway_membership;
    let path = tmp_db_path("gateway_nickname");
    let database = Db::open(&path, 2).unwrap();
    enroll_guild(&database, 10, 101).await.unwrap();
    patch_settings(&database, 10, None, Some(Some("new name".into())), None)
        .await
        .unwrap();
    let job = pending_nickname_reconciles(&database, i64::MAX, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(
        observe_gateway_membership(&database, 10, None, false, false)
            .await
            .is_err()
    );
    observe_gateway_membership(&database, 10, Some(101), true, false)
        .await
        .unwrap();
    observe_gateway_membership(&database, 10, Some(100), false, false)
        .await
        .unwrap();
    assert!(
        nickname_reconcile_can_dispatch(&database, &job)
            .await
            .unwrap()
    );
    observe_gateway_membership(&database, 10, Some(101), false, false)
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
