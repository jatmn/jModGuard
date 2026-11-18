use super::migration_fixtures::{schema_dump, user_version};
use super::*;

fn options(path: &Path, migrate: bool) -> DbOpenOptions {
    DbOpenOptions {
        read_only: false,
        path: path.to_owned(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate,
        default_prefix: "!".into(),
        retention_days: 7,
    }
}

#[test]
fn fresh_schema_has_no_retired_history_and_preserves_retained_records_on_reopen() {
    let path = tmp_db_path("fresh_schema");
    let db = Db::open(&path, 2).unwrap();
    db.with_conn_sync(|conn| {
        let retired: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('messages','members','users','mutes')",
            (), |r| Ok(r.get(0)?),
        )?;
        assert_eq!(retired, 0);
        assert_eq!(meta_string_conn(conn, "schema_family")?.as_deref(), Some(SCHEMA_FAMILY));
        Ok(())
    }).unwrap();
    let warn = tokio_test_block_on(add_warn(&db, 42, 100, 200, "retained audit")).unwrap();
    tokio_test_block_on(record_ban(&db, 42, 100, Some(200), "retained audit")).unwrap();
    drop(db);
    let api = Db::open_with(options(&path, false)).unwrap();
    assert_eq!(
        tokio_test_block_on(list_warns(&api, 42, 100)).unwrap()[0].id,
        warn
    );
    assert_eq!(
        tokio_test_block_on(list_bans(&api, 42, 10)).unwrap().len(),
        1
    );
    assert!(
        tokio_test_block_on(list_bans(&api, 43, 10))
            .unwrap()
            .is_empty()
    );
    drop(api);
    cleanup_db(&path);
}

#[test]
fn old_or_foreign_databases_are_rejected_without_changing_their_rows() {
    for (label, sql) in [
        (
            "old",
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES ('bootstrap_complete','5'); PRAGMA user_version=5;",
        ),
        // Underscore must be literal when ignoring SQLite-owned objects.
        (
            "foreign",
            "CREATE TABLE sqliteXforeign (value TEXT); INSERT INTO sqliteXforeign VALUES ('keep');",
        ),
        (
            "wrong_family",
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES ('schema_family','another-app'); PRAGMA user_version=1;",
        ),
    ] {
        let path = tmp_db_path(label);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch_bounded(sql, 64 * 1024 * 1024).unwrap();
        let before: String = conn
            .query_row(
                "SELECT group_concat(sql, ';') FROM sqlite_master",
                (),
                |r| Ok(r.get(0)?),
            )
            .unwrap();
        for migrate in [true, false] {
            let error = Db::open_with(options(&path, migrate))
                .err()
                .expect("must refuse adoption");
            assert!(
                error.downcast_ref::<UnsupportedSchema>().is_some(),
                "{error:#}"
            );
            assert!(!is_startup_retryable(&error, true));
            assert!(!is_startup_retryable(&error, false));
        }
        let after: String = conn
            .query_row(
                "SELECT group_concat(sql, ';') FROM sqlite_master",
                (),
                |r| Ok(r.get(0)?),
            )
            .unwrap();
        assert_eq!(before, after);
        let value: String = if label == "foreign" {
            conn.query_row("SELECT value FROM sqliteXforeign", (), |r| Ok(r.get(0)?))
                .unwrap()
        } else {
            conn.query_row("SELECT value FROM meta", (), |r| Ok(r.get(0)?))
                .unwrap()
        };
        assert_eq!(
            value,
            match label {
                "old" => "5",
                "foreign" => "keep",
                _ => "another-app",
            }
        );
        drop(conn);
        cleanup_db(&path);
    }
}

#[test]
fn newer_or_invalid_versions_refuse_before_bootstrap_changes() {
    for version in [0, -1, CORE_SCHEMA_VERSION + 1] {
        let path = tmp_db_path("schema_version_refusal");
        let db = Db::open(&path, 2).unwrap();
        db.with_conn_sync(move |conn| conn.pragma_update(None, "user_version", version))
            .unwrap();
        for migrate in [true, false] {
            let error = Db::open_with(options(&path, migrate))
                .err()
                .expect("must refuse version");
            assert!(
                error.downcast_ref::<UnsupportedSchema>().is_some(),
                "{error:#}"
            );
        }
        db.with_conn_sync(move |conn| {
            let actual: i64 = conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
            assert_eq!(actual, version);
            assert_eq!(
                meta_string_conn(conn, META_BOOTSTRAP_COMPLETE)?.as_deref(),
                Some(CORE_SCHEMA_VERSION.to_string().as_str())
            );
            Ok(())
        })
        .unwrap();
        drop(db);
        cleanup_db(&path);
    }
}

#[test]
fn first_schema_and_family_marker_rollback_together() {
    let path = tmp_db_path("schema_atomic");
    let conn = Connection::open(&path).unwrap();
    {
        let tx = conn.transaction_primary_write().unwrap();
        tx.execute_batch_bounded(MIGRATIONS[0].1, 64 * 1024 * 1024)
            .unwrap();
        tx.pragma_update(None, "user_version", CORE_BASELINE_VERSION)
            .unwrap();
        // Interrupted initializer before commit: dropping rolls back both.
    }
    assert_eq!(
        conn.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name NOT GLOB 'sqlite_*'",
            (),
            |r| Ok(r.get::<i64>(0)?)
        )
        .unwrap(),
        0
    );
    drop(conn);
    let db = Db::open(&path, 2).unwrap();
    drop(db);
    cleanup_db(&path);
}

#[test]
fn bot_resumes_each_committed_schema_step_before_api_opens() {
    for completed in 1..=MIGRATIONS.len() {
        let path = tmp_db_path("partial_schema_bootstrap");
        let conn = Connection::open(&path).unwrap();
        apply_core_steps(&conn, &MIGRATIONS[..completed]);
        drop(conn);
        let error = Db::open_with(options(&path, false))
            .err()
            .expect("API must wait for completed bot bootstrap");
        assert!(is_schema_not_ready(&error), "{error:#}");
        drop(Db::open_with(options(&path, true)).unwrap());
        let api = Db::open_with(options(&path, false)).unwrap();
        let parts = tokio_test_block_on(load_config_parts(&api, 42)).unwrap();
        assert_eq!(parts.rules.len(), crate::types::RULE_IDS.len());
        assert_eq!(parts.templates.len(), crate::types::EVENT_TYPES.len());
        drop(api);
        cleanup_db(&path);
    }
}

const V0_2_1_CORE_VERSION: i64 = 26;

fn replay_released_core(conn: &Connection, version: i64) {
    apply_historical_core(conn, 0, CORE_BASELINE_VERSION);
    let end = MIGRATIONS.partition_point(|&(at, _)| at <= version);
    // Versions 24–26 still use the original v0.2.1 SQL, verified against the tag.
    apply_core_steps(conn, &MIGRATIONS[1..end]);
}

#[test]
fn fresh_core_matches_the_v0_2_1_replay_and_later_steps() {
    let fresh = tmp_db_path("core_baseline_fresh");
    drop(Db::open_with(options(&fresh, true)).unwrap());
    let replay = tmp_db_path("core_full_replay");
    let conn = Connection::open(&replay).unwrap();
    replay_released_core(&conn, V0_2_1_CORE_VERSION);
    let start = MIGRATIONS.partition_point(|&(at, _)| at <= V0_2_1_CORE_VERSION);
    apply_core_steps(&conn, &MIGRATIONS[start..]);
    let fresh_conn = Connection::open(&fresh).unwrap();
    assert_eq!(user_version(&fresh_conn), user_version(&conn));
    assert_eq!(schema_dump(&fresh_conn), schema_dump(&conn));
    drop((fresh_conn, conn));
    cleanup_db(&fresh);
    cleanup_db(&replay);
}

#[test]
fn released_core_databases_upgrade_with_data_and_reopen() {
    let fresh = tmp_db_path("core_baseline_comparison");
    drop(Db::open_with(options(&fresh, true)).unwrap());
    let expected = schema_dump(&Connection::open(&fresh).unwrap());
    cleanup_db(&fresh);
    for released in [CORE_BASELINE_VERSION, V0_2_1_CORE_VERSION] {
        let path = tmp_db_path("released_core_upgrade");
        let opts = options(&path, true);
        let conn = Connection::open(&path).unwrap();
        replay_released_core(&conn, released);
        write_meta_string_conn(&conn, META_BOOTSTRAP_COMPLETE, &released.to_string()).unwrap();
        conn.execute_batch_bounded(
            r#"INSERT INTO guild_settings VALUES (42, '{"prefix":"?"}');
               INSERT INTO warns (id, guild_id, user_id, moderator_id, reason, created_at)
               VALUES (7, 42, 100, 200, 'retained moderation', '2026-01-01');
               INSERT INTO bans (id, guild_id, user_id, reason, created_at)
               VALUES (8, 42, 101, 'retained ban', '2026-01-02');"#,
            1024 * 1024,
        )
        .unwrap();
        drop(conn);
        let control = UpdateControl::open(&opts).unwrap().unwrap();
        assert_eq!(
            tokio_test_block_on(control.schema_version()).unwrap(),
            released
        );
        assert!(tokio_test_block_on(control.upgrade()).unwrap().is_none());
        drop(control);
        let api_opts = DbOpenOptions {
            read_only: true,
            ..options(&path, false)
        };
        assert!(is_schema_not_ready(
            &Db::open_with(api_opts.clone()).err().unwrap()
        ));

        for _ in 0..2 {
            drop(Db::open_with(opts.clone()).unwrap());
            let api = Db::open_with(api_opts.clone()).unwrap();
            api.with_conn_sync(|conn| {
                assert_eq!(user_version(conn), CORE_SCHEMA_VERSION);
                assert_eq!(schema_dump(conn), expected);
                let retained: (String, String, String) = conn.query_row(
                    "SELECT (SELECT settings FROM guild_settings WHERE guild_id=42),
                            (SELECT reason FROM warns WHERE id=7 AND guild_id=42 AND user_id=100 AND moderator_id=200),
                            (SELECT list_reason FROM bans WHERE id=8 AND guild_id=42 AND user_id=101)",
                    (), |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                assert_eq!(retained, (r#"{"prefix":"?"}"#.into(), "retained moderation".into(), "retained ban".into()));
                Ok(())
            }).unwrap();
        }
        let control = UpdateControl::open(&opts).unwrap().unwrap();
        assert_eq!(
            tokio_test_block_on(control.schema_version()).unwrap(),
            CORE_SCHEMA_VERSION
        );
        drop(control);
        cleanup_db(&path);
    }
}

#[test]
fn pre_v0_2_0_core_is_refused_without_schema_or_data_changes() {
    let path = tmp_db_path("unsupported_core_version");
    let conn = Connection::open(&path).unwrap();
    let released = CORE_BASELINE_VERSION - 1;
    apply_historical_core(&conn, 0, released);
    write_meta_string_conn(&conn, META_BOOTSTRAP_COMPLETE, &released.to_string()).unwrap();
    let before = schema_dump(&conn);
    for migrate in [true, false] {
        let error = Db::open_with(options(&path, migrate)).err().unwrap();
        assert!(error.is::<UnsupportedSchema>(), "{error:#}");
        assert!(
            format!("{error:#}").contains("upgrade to v0.2.0 first"),
            "{error:#}"
        );
    }
    assert_eq!(schema_dump(&conn), before);
    assert_eq!(user_version(&conn), released);
    assert_eq!(
        meta_string_conn(&conn, META_BOOTSTRAP_COMPLETE).unwrap(),
        Some(released.to_string())
    );
    drop(conn);
    cleanup_db(&path);
}

#[test]
fn migrating_one_version_takes_only_baseline_and_migration_writer_locks() {
    let path = tmp_db_path("migrate_skips_applied");
    let prior = MIGRATIONS.len() - 1;
    let conn = Connection::open(&path).unwrap();
    apply_core_steps(&conn, &MIGRATIONS[..prior]);
    drop(conn);
    let probe = Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let mut conn = Connection::open(&path).unwrap();
    migrate_conn(&mut conn).unwrap();
    assert_eq!(
        probe.immediate(),
        2,
        "baseline read plus the one pending migration"
    );
    assert_eq!(
        conn.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        CORE_SCHEMA_VERSION
    );
    drop(conn);
    cleanup_db(&path);
}

#[test]
fn ban_list_migration_admits_core_store_with_large_reusable_allocation() {
    let path = tmp_db_path("ban_list_migration_reusable_pages");
    {
        let conn = open_core_26(&path);
        conn.execute_batch_bounded(
            r#"INSERT INTO bans (id, guild_id, user_id, reason, created_at) VALUES
                (7, 10, 123, 'original', '2026-01-01T00:00:00Z'),
                (8, 10, 124, 'untouched', '2026-01-01T00:00:00Z');
            INSERT INTO events (guild_id, event_type, actor_id, data, created_at) VALUES
                (10, 'ban_reason_edit', 80, '{"ban_id":7,"reason":"old"}', '2026-01-02T00:00:00Z'),
                (10, 'ban_reason_edit', 81, '{"ban_id":7,"reason":"Latest"}', '2026-01-03T00:00:00Z'),
                (11, 'ban_reason_edit', 82, '{"ban_id":7,"reason":"foreign"}', '2026-01-04T00:00:00Z');
            CREATE TABLE allocation_fixture (payload BLOB);
            INSERT INTO allocation_fixture VALUES (zeroblob(280000000));
            DROP TABLE allocation_fixture;"#,
            300_000_000,
        )
        .unwrap();
        let allocation = crate::capacity::sample(&conn).unwrap();
        let reusable_pages: i64 = conn
            .query_row("PRAGMA freelist_count", (), |row| Ok(row.get(0)?))
            .unwrap();
        assert!(reusable_pages as u64 * allocation.page_size >= 280_000_000);
        assert!(allocation.allocated_bytes < crate::capacity::GROWTH_STOP_BYTES);
    }

    let db = Db::open(&path, 2).expect("core-26 store with reusable pages should upgrade");
    db.with_conn_sync(|conn| {
        let version: i64 = conn.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))?;
        assert_eq!(version, 27);
        let edited: (String, String, i64, String, String) = conn.query_row(
            "SELECT reason, list_edited_reason, list_reason_edited_by, list_reason_edited_at, list_reason
             FROM bans WHERE id=7",
            (),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        assert_eq!(
            edited,
            ("original".into(), "Latest".into(), 81, "2026-01-03T00:00:00Z".into(), "latest".into())
        );
        let untouched: (Option<String>, String) = conn.query_row(
            "SELECT list_edited_reason, list_reason FROM bans WHERE id=8",
            (),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(untouched, (None, "untouched".into()));
        Ok(())
    })
    .unwrap();
    drop(db);
    cleanup_db(&path);
}

#[test]
fn ban_list_migration_rejects_payload_beyond_growth_budget_before_ddl() {
    let path = tmp_db_path("ban_list_migration_capacity");
    let conn = open_core_26(&path);
    conn.execute_batch_bounded(
        "INSERT INTO bans (guild_id, user_id, reason, created_at)
         VALUES (10, 123, hex(zeroblob(1048576)), '2026-01-01T00:00:00Z');",
        4 * 1024 * 1024,
    )
    .unwrap();
    let allocated = crate::capacity::allocated_bytes(&conn).unwrap();
    drop(conn);

    // Leave room for the fixed DDL allowance, but not the legacy reason index entry
    // and its conservative page allowance.
    let growth_limit = allocated + 8 * 1024 * 1024;
    let error = crate::db_compat::with_test_default_capacity_limits(
        growth_limit,
        crate::capacity::RECLAIM_STOP_BYTES,
        || {
            Db::open(&path, 2)
                .err()
                .expect("migration must refuse growth")
        },
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("migration 27: application database capacity budget exhausted"),
        "{message}"
    );
    let conn = Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))
        .unwrap();
    assert_eq!(version, 26);
    let columns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_xinfo('bans') WHERE name GLOB 'list_*'",
            (),
            |row| Ok(row.get(0)?),
        )
        .unwrap();
    assert_eq!(columns, 0);
    drop(conn);
    cleanup_db(&path);
}

fn open_core_26(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    apply_core_steps(&conn, &MIGRATIONS[..MIGRATIONS.len() - 1]);
    write_meta_string_conn(&conn, META_BOOTSTRAP_COMPLETE, "26").unwrap();
    conn
}

#[test]
fn action_schema_upgrade_preserves_version_five_configuration_and_audits() {
    let path = tmp_db_path("queue_upgrade");
    let conn = Connection::open(&path).unwrap();
    apply_historical_core(&conn, 0, 5);
    conn.execute_batch_bounded("INSERT INTO log_channels(guild_id,event_type,channel_id) VALUES(42,'member_join',123);
        INSERT INTO warns(guild_id,user_id,moderator_id,reason,created_at) VALUES(42,100,200,'keep audit','2026-09-07');", 64 * 1024 * 1024).unwrap();
    apply_historical_core(&conn, 5, CORE_BASELINE_VERSION);
    drop(conn);
    assert!(is_schema_not_ready(
        &Db::open_with(options(&path, false)).err().unwrap()
    ));
    drop(Db::open_with(options(&path, true)).unwrap());
    let api = Db::open_with(options(&path, false)).unwrap();
    assert_eq!(
        tokio_test_block_on(list_log_channels(&api, 42)).unwrap()[0].1,
        123
    );
    assert_eq!(
        tokio_test_block_on(list_warns(&api, 42, 100)).unwrap()[0]
            .reason
            .as_deref(),
        Some("keep audit")
    );
    assert!(
        tokio_test_block_on(recent_bot_actions(&api, 42))
            .unwrap()
            .is_empty()
    );
    drop(api);
    cleanup_db(&path);
}

#[test]
fn one_active_ban_migration_collapses_existing_duplicates() {
    let path = tmp_db_path("one_active_ban_migration");
    let mut conn = Connection::open(&path).unwrap();
    let prior = 17; // Test 0018 even after later migrations are added.
    apply_historical_core(&conn, 0, prior);
    conn.execute_batch_bounded(
        "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active) VALUES
            (10, 55, 9, 'spam', '2024-01-01T00:00:00Z', 1),
            (10, 55, NULL, 'Reason unavailable', '2024-01-01T00:00:01Z', 1),
            (10, 56, NULL, 'Reason unavailable', '2024-01-02T00:00:00Z', 1),
            (10, 56, NULL, 'Reason unavailable', '2024-01-02T00:00:01Z', 1),
            (10, 57, 3, 'ended', '2024-01-03T00:00:00Z', 0),
            (10, 57, 4, 'fresh', '2024-01-04T00:00:00Z', 1);",
        64 * 1024 * 1024,
    )
    .unwrap();
    apply_historical_core(&conn, prior, CORE_BASELINE_VERSION);
    migrate_conn(&mut conn).unwrap();
    let row = |sql: &str| -> (Option<i64>, i64) {
        conn.query_row(sql, (), |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    };
    assert_eq!(
        row("SELECT moderator_id, active FROM bans WHERE user_id=55 AND reason='spam'"),
        (Some(9), 1)
    );
    assert_eq!(
        row(
            "SELECT moderator_id, active FROM bans WHERE user_id=56 AND reason='Reason unavailable'"
        ),
        (None, 1)
    );
    assert_eq!(
        row("SELECT moderator_id, active FROM bans WHERE user_id=57 AND reason='ended'"),
        (Some(3), 0)
    );
    assert_eq!(
        row("SELECT moderator_id, active FROM bans WHERE user_id=57 AND reason='fresh'"),
        (Some(4), 1)
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM bans", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        4
    );
    let error = conn
        .execute_batch_bounded(
            "INSERT INTO bans (guild_id, user_id, moderator_id, reason, created_at, active) VALUES (10, 55, 1, 'second', '2024-05-01T00:00:00Z', 1);",
            64 * 1024 * 1024,
        )
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("UNIQUE constraint failed") || text.contains("idx_bans_one_active"),
        "{text}"
    );
    drop(conn);
    cleanup_db(&path);
}

#[test]
fn builtin_command_access_migration_drops_legacy_rows_and_is_idempotent() {
    let path = tmp_db_path("builtin_command_access_migration");
    let mut conn = Connection::open(&path).unwrap();
    let prior = 20; // 0021 deletes built-in and reserved-alias command rows.
    apply_historical_core(&conn, 0, prior);
    let ban = crate::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "ban")
        .unwrap();
    let voice = crate::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "voice")
        .unwrap();
    let mut legacy_ban = ban.clone();
    legacy_ban.enabled = false;
    legacy_ban.permission = "owner".into();
    legacy_ban.roles = vec![555];
    legacy_ban.channels = vec![777];
    legacy_ban.cooldown_secs = 42;
    legacy_ban.description = "legacy ban".into();
    let mut legacy_voice = voice;
    legacy_voice.enabled = false;
    legacy_voice.permission = "owner".into();
    legacy_voice.cooldown_secs = 9;
    legacy_voice.description = "legacy voice".into();
    legacy_voice.behavior = crate::types::Behavior::Say;
    let mut alias_row = ban;
    alias_row.name = "yeet".into();
    alias_row.enabled = false;
    alias_row.permission = "owner".into();
    let mut custom = crate::defaults::custom_command_fixture("greet");
    custom.aliases = vec!["status".into(), "mine".into()];
    let userinfo = crate::defaults::default_commands()
        .into_iter()
        .find(|cmd| cmd.name == "userinfo")
        .unwrap();
    let mut legacy_userinfo = userinfo;
    legacy_userinfo.enabled = false;
    legacy_userinfo.permission = "owner".into();
    legacy_userinfo.cooldown_secs = 15;
    legacy_userinfo.description = "legacy userinfo".into();
    let inserts = [
        (0_i64, "ban", serde_json::to_string(&legacy_ban).unwrap()),
        (10_i64, "Ban", serde_json::to_string(&legacy_ban).unwrap()),
        (
            10_i64,
            "voice",
            serde_json::to_string(&legacy_voice).unwrap(),
        ),
        (10_i64, "yeet", serde_json::to_string(&alias_row).unwrap()),
        (
            0_i64,
            "userinfo",
            serde_json::to_string(&legacy_userinfo).unwrap(),
        ),
        (
            10_i64,
            "UserInfo",
            serde_json::to_string(&legacy_userinfo).unwrap(),
        ),
        (10_i64, "greet", serde_json::to_string(&custom).unwrap()),
    ];
    for (guild_id, name, data) in inserts {
        conn.execute_bounded(
            "INSERT INTO commands (guild_id, name, data) VALUES (?1, ?2, ?3)",
            params![guild_id, name, data],
            64 * 1024 * 1024,
        )
        .unwrap();
    }
    apply_historical_core(&conn, prior, CORE_BASELINE_VERSION);
    migrate_conn(&mut conn).unwrap();
    let count = |conn: &Connection, sql: &str| -> i64 {
        conn.query_row(sql, (), |row| Ok(row.get(0)?)).unwrap()
    };
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM commands WHERE lower(name) IN ('ban','voice','yeet','userinfo')"
        ),
        0,
        "built-in and reserved-alias rows are removed"
    );
    let greet: String = conn
        .query_row(
            "SELECT data FROM commands WHERE guild_id=10 AND name='greet'",
            (),
            |row| Ok(row.get(0)?),
        )
        .unwrap();
    assert!(greet.contains("status"), "{greet}");
    assert!(greet.contains("mine"), "{greet}");
    let version: i64 = conn
        .query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))
        .unwrap();
    assert_eq!(version, CORE_SCHEMA_VERSION);
    let changes_before: i64 = count(&conn, "SELECT total_changes()");
    conn.execute_batch_bounded(
        super::migration_fixtures::HISTORICAL_CORE[prior as usize].1,
        64 * 1024 * 1024,
    )
    .unwrap();
    conn.execute_batch_bounded(
        super::migration_fixtures::HISTORICAL_CORE[21].1,
        64 * 1024 * 1024,
    )
    .unwrap();
    assert_eq!(
        count(&conn, "SELECT total_changes()"),
        changes_before,
        "replaying the migration SQL changes nothing"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM commands WHERE name='greet'"),
        1
    );
    migrate_conn(&mut conn).unwrap();
    let version_after: i64 = conn
        .query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?))
        .unwrap();
    assert_eq!(version_after, version);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM commands WHERE name='greet'"),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM commands WHERE lower(name) IN ('ban','voice','yeet','userinfo')"
        ),
        0
    );
    drop(conn);
    for _ in 0..2 {
        let db = Db::open(&path, 2).unwrap();
        db.with_conn_sync(|conn| {
            let global_ban: i64 = conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE guild_id=0 AND name='ban'",
                (),
                |row| Ok(row.get(0)?),
            )?;
            assert_eq!(global_ban, 1, "seed restores the shipped global ban row");
            let global_userinfo: i64 = conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE guild_id=0 AND name='userinfo'",
                (),
                |row| Ok(row.get(0)?),
            )?;
            assert_eq!(
                global_userinfo, 1,
                "seed restores the shipped global userinfo row"
            );
            let guild_legacy: i64 = conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE guild_id=10 AND lower(name) IN ('ban','voice','yeet','userinfo')",
                (),
                |row| Ok(row.get(0)?),
            )?;
            assert_eq!(guild_legacy, 0);
            let greet: i64 = conn.query_row(
                "SELECT COUNT(*) FROM commands WHERE name='greet'",
                (),
                |row| Ok(row.get(0)?),
            )?;
            assert_eq!(greet, 1);
            Ok(())
        })
        .unwrap();
        drop(db);
    }
    cleanup_db(&path);
}
