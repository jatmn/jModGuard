use jmodguard::db::{
    Db, DbOpenOptions, add_badword, add_warn, bump_config_version, get_config_version,
    get_settings_primary, insert_event, list_badwords, list_warns, load_config_parts_with_version,
    patch_settings, query_events, test_drop_next_commit_response,
};
use jmodguard::state::ConfigCache;
use std::path::PathBuf;
use std::time::Duration;

fn remote_options(migrate: bool) -> DbOpenOptions {
    DbOpenOptions {
        // Ignored by the remote backend, but retained for the common config contract.
        path: PathBuf::from("data/jmodguard.db"),
        url: std::env::var("JMODGUARD_REMOTE_TEST_URL")
            .expect("JMODGUARD_REMOTE_TEST_URL must point to a disposable libSQL database"),
        token: std::env::var("JMODGUARD_REMOTE_TEST_TOKEN")
            .expect("JMODGUARD_REMOTE_TEST_TOKEN must be set"),
        pool_size: 4,
        migrate,
        default_prefix: "!".into(),
        retention_days: 30,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a disposable remote libSQL server"]
async fn bot_bootstrap_and_api_share_remote_database() {
    let bot = Db::open_with(remote_options(true)).expect("bot should bootstrap remote database");
    let before = get_config_version(&bot)
        .await
        .expect("bot should read config version");
    bump_config_version(&bot)
        .await
        .expect("bot should write config version");

    let api = Db::open_with(remote_options(false)).expect("API should open bootstrapped database");
    let observed = get_config_version(&api)
        .await
        .expect("API should read config version");
    assert_eq!(observed, before + 1);

    let error = bot
        .with_conn(|conn| -> anyhow::Result<()> {
            let tx = conn.transaction()?;
            tx.execute_batch("UPDATE meta SET value = '999999999' WHERE key = 'config_version'")?;
            anyhow::bail!("forced failure after remote transaction write");
        })
        .await
        .expect_err("transaction body must fail");
    assert!(error.to_string().contains("forced failure"));
    assert_eq!(
        get_config_version(&api).await.unwrap(),
        before + 1,
        "failed transaction must not persist its sentinel write"
    );

    tokio::time::timeout(Duration::from_secs(2), bump_config_version(&bot))
        .await
        .expect("next write stalled behind an unrolled-back transaction")
        .expect("next write should succeed after rollback");
    assert_eq!(get_config_version(&bot).await.unwrap(), before + 2);

    // Commit the real remote transaction, then simulate losing its response.
    // The receipt lookup must establish success on a fresh primary-routed
    // connection rather than reporting an error that invites a duplicate retry.
    test_drop_next_commit_response();
    let committed = patch_settings(&bot, 42, Some(Some("?".into())), None, None)
        .await
        .expect("lost COMMIT response should reconcile to the committed outcome");
    assert_eq!(committed.prefix, "?");
    assert_eq!(
        get_settings_primary(&api, 42).await.unwrap().prefix,
        "?",
        "the next authoritative read must observe the reconciled mutation"
    );

    test_drop_next_commit_response();
    let id = add_badword(&bot, 42, "receipt-sentinel", None)
        .await
        .expect("a non-idempotent insert must also reconcile to success");
    let rows = list_badwords(&api, 42).await.unwrap();
    assert_eq!(
        rows.iter().filter(|row| row.id == id).count(),
        1,
        "lost COMMIT response must not invite a duplicate insert retry"
    );

    // Ordinary config reads may land on a stale replica immediately after a
    // write. Inject an actual pre-write snapshot and prove the cache falls
    // back to one primary-routed snapshot instead of dropping the request.
    let guild_id = 4_242_u64;
    let stale = load_config_parts_with_version(&bot, guild_id as i64)
        .await
        .expect("capture pre-write config snapshot");
    let cache = ConfigCache::new(stale.0, stale.1.clone());
    patch_settings(
        &bot,
        guild_id as i64,
        Some(Some("replica-safe".into())),
        None,
        None,
    )
    .await
    .expect("commit config change before stale replica read");
    bot.test_inject_next_config_snapshot(stale);
    let loaded = cache
        .get(&bot, guild_id)
        .await
        .expect("stale replica must trigger a primary snapshot, not a config-load failure");
    assert_eq!(loaded.settings.prefix, "replica-safe");

    // Insert identity belongs to the transaction stream that executed the
    // statement. Verify both runtime insert APIs return their persisted IDs.
    let event_id = insert_event(
        &bot,
        42,
        "remote-id-sentinel",
        Some(100),
        Some(200),
        &serde_json::json!({"source": "remote-smoke"}),
    )
    .await
    .expect("remote event insert should return its transaction-owned id");
    assert!(event_id > 0);
    assert!(
        query_events(&api, 42, Some("remote-id-sentinel"), None, 20)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == event_id)
    );

    let warn_id = add_warn(&bot, 42, 100, 200, "remote-id-sentinel")
        .await
        .expect("remote warn insert should return its transaction-owned id");
    assert!(warn_id > 0);
    assert!(
        list_warns(&api, 42, 100)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == warn_id)
    );
}
