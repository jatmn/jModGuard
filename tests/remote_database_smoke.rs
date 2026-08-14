use jmodguard::db::{
    Db, DbOpenOptions, add_badword, bump_config_version, get_config_version, get_settings_primary,
    list_badwords, patch_settings, test_drop_next_commit_response,
};
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
}
