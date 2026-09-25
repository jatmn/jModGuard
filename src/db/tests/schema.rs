use super::*;

fn options(path: &Path, migrate: bool) -> DbOpenOptions {
    DbOpenOptions {
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
    for version in [0, -1, MIGRATIONS.len() as i64 + 1] {
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
                Some(MIGRATIONS.len().to_string().as_str())
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
        let tx = conn.transaction_primary_read().unwrap();
        tx.execute_batch_bounded(MIGRATIONS[0], 64 * 1024 * 1024)
            .unwrap();
        tx.pragma_update(None, "user_version", 1).unwrap();
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
        for (index, sql) in MIGRATIONS[..completed].iter().enumerate() {
            let tx = conn.transaction_primary_read().unwrap();
            tx.execute_batch_bounded(sql, 64 * 1024 * 1024).unwrap();
            tx.pragma_update(None, "user_version", (index + 1) as i64)
                .unwrap();
            tx.commit().unwrap();
        }
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

#[test]
fn action_schema_upgrade_preserves_version_five_configuration_and_audits() {
    let path = tmp_db_path("queue_upgrade");
    let conn = Connection::open(&path).unwrap();
    for (index, sql) in MIGRATIONS[..5].iter().enumerate() {
        let tx = conn.transaction_primary_read().unwrap();
        tx.execute_batch_bounded(sql, 64 * 1024 * 1024).unwrap();
        tx.pragma_update(None, "user_version", (index + 1) as i64)
            .unwrap();
        tx.commit().unwrap();
    }
    conn.execute_batch_bounded("INSERT INTO log_channels(guild_id,event_type,channel_id) VALUES(42,'member_join',123);
        INSERT INTO warns(guild_id,user_id,moderator_id,reason,created_at) VALUES(42,100,200,'keep audit','2026-09-07');", 64 * 1024 * 1024).unwrap();
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
