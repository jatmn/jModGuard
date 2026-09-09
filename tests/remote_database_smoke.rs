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
    // Bootstrap has no mutation-receipt table before its migrations run. A
    // lost response for its first primary transaction must be verified from a
    // fresh primary snapshot rather than making the bot exit.
    test_drop_next_commit_response();
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

    exercise_action_storage(&bot, &api).await;
    exercise_membership_storage(&bot, &api).await;
    exercise_captured_action_membership(&bot, &api).await;
    exercise_nickname_membership(&bot, &api).await;
    exercise_configuration_scopes(&bot, &api).await;

    // The fresh schema must not grow a chat or member archive on remote SQL.
    // Exercise the same check on the remote transport as on local SQLite.
    api.with_conn(|conn| {
        use jmodguard::db::SqlExecutor;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('messages','members','users','mutes')",
            (), |r| Ok(r.get(0)?),
        )?;
        anyhow::ensure!(count == 0, "retired history tables exist");
        Ok(())
    }).await.unwrap();
}

// Queue storage only: this test never sends a Discord request.
async fn exercise_action_storage(bot: &Db, api: &Db) {
    use jmodguard::bot_actions::{Action, Limits};
    use jmodguard::db::{
        begin_bot_action, bot_action, claim_bot_action, enqueue_bot_action, finish_bot_action,
    };
    jmodguard::db::enroll_guild(bot, 900, 100).await.unwrap();
    let version = get_config_version(api).await.unwrap();
    let action = Action::Unban {
        user_id: 901,
        reason: "remote storage smoke".into(),
    };
    test_drop_next_commit_response();
    let first = enqueue_bot_action(
        api,
        900,
        902,
        "1000_remote",
        action.clone(),
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    let retry = enqueue_bot_action(
        api,
        900,
        902,
        "1000_remote",
        action,
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.id, retry.id);
    assert!(bot_action(bot, 901, &first.id).await.unwrap().is_none());
    let claim = claim_bot_action(bot, 1001).await.unwrap().unwrap();
    assert_eq!(claim.id, first.id);
    test_drop_next_commit_response();
    assert!(begin_bot_action(bot, &claim, 1001).await.unwrap());
    assert!(!begin_bot_action(bot, &claim, 1001).await.unwrap());
    test_drop_next_commit_response();
    assert!(
        finish_bot_action(bot, &claim, "succeeded", "synthetic_storage_result")
            .await
            .unwrap()
    );
    assert!(
        !finish_bot_action(bot, &claim, "succeeded", "duplicate")
            .await
            .unwrap()
    );
    assert_eq!(
        bot_action(api, 900, &first.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "succeeded"
    );
    assert_eq!(
        query_events(api, 900, Some("member_unban"), None, 20)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(version, get_config_version(api).await.unwrap());
}

async fn exercise_membership_storage(bot: &Db, api: &Db) {
    use jmodguard::db::{enroll_guild, guild_enrollment, retire_guild_enrollment};
    let version = get_config_version(api).await.unwrap();
    test_drop_next_commit_response();
    let first = enroll_guild(bot, 800, 100).await.unwrap().unwrap();
    assert_eq!(
        guild_enrollment(api, 800).await.unwrap(),
        Some(first.clone())
    );
    // A rollback can hide executed receipt writes from row-count assertions.
    // Exercise the remote-only receipt path and reject the INSERT itself.
    bot.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_matching_receipt BEFORE INSERT ON mutation_receipts
             BEGIN SELECT RAISE(ABORT, 'matching observation wrote a receipt'); END",
        )
    })
    .await
    .unwrap();
    assert_eq!(
        enroll_guild(bot, 800, 100).await.unwrap(),
        Some(first.clone())
    );
    assert!(enroll_guild(bot, 800, 99).await.unwrap().is_none());
    bot.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_matching_receipt"))
        .await
        .unwrap();
    assert!(guild_enrollment(api, 801).await.unwrap().is_none());
    test_drop_next_commit_response();
    retire_guild_enrollment(bot, 800, 100).await.unwrap();
    assert!(enroll_guild(bot, 800, 100).await.unwrap().is_none());
    assert!(guild_enrollment(api, 800).await.unwrap().is_none());
    let next = enroll_guild(bot, 800, 101).await.unwrap().unwrap();
    assert_ne!(first.id, next.id);
    retire_guild_enrollment(bot, 800, 100).await.unwrap();
    assert_eq!(guild_enrollment(api, 800).await.unwrap(), Some(next));
    assert_eq!(get_config_version(api).await.unwrap(), version);
}

async fn exercise_captured_action_membership(bot: &Db, api: &Db) {
    use jmodguard::bot_actions::{Action, Limits};
    use jmodguard::db::*;
    let old = enroll_guild(bot, 700, 100).await.unwrap().unwrap();
    let first = enqueue_enrolled_bot_action(
        api,
        &old,
        42,
        "1000_bound",
        Action::Unban {
            user_id: 99,
            reason: "storage test".into(),
        },
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    let claim = claim_bot_action(bot, 1001).await.unwrap().unwrap();
    assert_eq!(claim.id, first.id);
    assert!(begin_bot_action(bot, &claim, 1001).await.unwrap());
    test_drop_next_commit_response();
    retire_guild_enrollment(bot, 700, 100).await.unwrap();
    assert!(
        !finish_bot_action(bot, &claim, "succeeded", "late")
            .await
            .unwrap()
    );
    let current = enroll_guild(bot, 700, 101).await.unwrap().unwrap();
    assert!(
        recent_enrolled_bot_actions(api, &old)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        cancel_enrolled_bot_action(api, &old, &first.id)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        recent_enrolled_bot_actions(api, &current)
            .await
            .unwrap()
            .is_empty()
    );
    api.with_conn(move |conn| {
        let state: String = conn.query_row(
            "SELECT state FROM bot_actions WHERE id=?1",
            [first.id],
            |r| Ok(r.get(0)?),
        )?;
        assert_eq!(state, "uncertain");
        Ok(())
    })
    .await
    .unwrap();
}

async fn exercise_nickname_membership(bot: &Db, api: &Db) {
    use jmodguard::db::*;
    enroll_guild(bot, 600, 100).await.unwrap();
    test_drop_next_commit_response();
    patch_settings(api, 600, None, Some(Some("first".into())), None)
        .await
        .unwrap();
    let old = pending_nickname_reconciles(bot, 0, 1)
        .await
        .unwrap()
        .remove(0);
    finish_nickname_reconcile(bot, &old, "dormant", "unknown_guild", "unknown", 1)
        .await
        .unwrap();
    test_drop_next_commit_response();
    enroll_guild(bot, 600, 100).await.unwrap();
    let resumed = pending_nickname_reconciles(bot, 0, 1)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(resumed.job_id, old.job_id);
    assert!(resumed.generation > old.generation);
    // Healthy repeats preserve the reviewed receipt-free observation behavior.
    bot.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_nickname_noop_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected noop receipt'); END")).await.unwrap();
    enroll_guild(bot, 600, 100).await.unwrap();
    bot.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_nickname_noop_receipt"))
        .await
        .unwrap();
    test_drop_next_commit_response();
    retire_guild_enrollment(bot, 600, 100).await.unwrap();
    assert!(
        !nickname_reconcile_can_dispatch(bot, &resumed)
            .await
            .unwrap()
    );
    enroll_guild(bot, 600, 101).await.unwrap();
    assert!(
        pending_nickname_reconciles(bot, i64::MAX, 1)
            .await
            .unwrap()
            .is_empty()
    );
    patch_settings(api, 600, None, Some(Some("new".into())), None)
        .await
        .unwrap();
    let current = pending_nickname_reconciles(bot, 0, 1)
        .await
        .unwrap()
        .remove(0);
    acknowledge_nickname_reconcile(bot, &resumed).await.unwrap();
    schedule_nickname_reconcile_retry(bot, &resumed, 1, "late", "late")
        .await
        .unwrap();
    assert!(
        nickname_reconcile_can_dispatch(bot, &current)
            .await
            .unwrap()
    );
    test_drop_next_commit_response();
    acknowledge_nickname_reconcile(bot, &current).await.unwrap();
    assert!(
        pending_nickname_reconciles(bot, i64::MAX, 1)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn exercise_configuration_scopes(bot: &Db, api: &Db) {
    use jmodguard::db::*;
    let source = ConfigurationScope::Guild(enroll_guild(bot, 500, 100).await.unwrap().unwrap());
    let destination =
        ConfigurationScope::Guild(enroll_guild(bot, 501, 100).await.unwrap().unwrap());
    test_drop_next_commit_response();
    patch_enrolled_settings(
        api,
        source.clone(),
        Some(Some("source".into())),
        Some(Some("name".into())),
        false,
    )
    .await
    .unwrap();
    // Empty patches still verify membership but must not insert recovery receipts.
    bot.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_config_noop_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected noop receipt'); END")).await.unwrap();
    let settings = patch_enrolled_settings(api, source.clone(), None, None, false)
        .await
        .unwrap();
    assert_eq!(settings.prefix, "source");
    let snapshot = load_enrolled_web_config_snapshot(api, source.clone())
        .await
        .unwrap();
    assert_eq!(snapshot.settings.settings.prefix, "source");
    assert!(snapshot.version >= api.required_config_version());
    bot.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_config_noop_receipt"))
        .await
        .unwrap();
    let write = ConfigurationWriteScope::new(source.clone(), source.clone()).unwrap();
    test_drop_next_commit_response();
    let word = add_enrolled_badword(api, write, "scope-sentinel", None)
        .await
        .unwrap();
    assert_eq!(
        list_badwords(api, 500)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.id == word)
            .count(),
        1
    );
    test_drop_next_commit_response();
    copy_enrolled_guild_config(
        api,
        source.clone(),
        destination.clone(),
        vec!["settings".into(), "badwords".into()],
    )
    .await
    .unwrap();
    assert_eq!(
        get_settings_primary(api, 501).await.unwrap().prefix,
        "source"
    );
    let jobs = pending_nickname_reconciles(bot, 0, 25).await.unwrap();
    assert!(
        jobs.iter()
            .any(|job| job.guild_id == 501 && job.nickname == "name")
    );
    retire_guild_enrollment(bot, 500, 100).await.unwrap();
    enroll_guild(bot, 500, 101).await.unwrap();
    let before = get_primary_versions(bot).await.unwrap();
    assert!(
        patch_enrolled_settings(api, source.clone(), None, None, false)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        copy_enrolled_guild_config(api, source.clone(), destination, vec!["all".into()])
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        load_enrolled_web_config_snapshot(api, source.clone())
            .await
            .err()
            .unwrap()
            .is::<EnrollmentChanged>()
    );
    let global = ConfigurationWriteScope::new(source, ConfigurationScope::Global).unwrap();
    assert!(
        upsert_enrolled_template(api, global, "scope-sentinel", "rejected")
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    let after = get_primary_versions(bot).await.unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(before.slash_revision, after.slash_revision);
    assert_eq!(
        get_settings_primary(api, 501).await.unwrap().prefix,
        "source"
    );
}
