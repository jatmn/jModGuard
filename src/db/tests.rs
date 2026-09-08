use super::*;

#[test]
fn remote_pool_admission_has_a_deadline() {
    let tmp = tmp_db_path("pool_admission_timeout");
    let mut db = Db::open(&tmp, 2).unwrap();
    db.permit_timeout = Some(Duration::from_millis(50));

    tokio_test_block_on(async {
        let first = Arc::clone(&db.permits).acquire_owned().await.unwrap();
        let second = Arc::clone(&db.permits).acquire_owned().await.unwrap();
        let error = db
            .with_conn(|_| Ok(()))
            .await
            .expect_err("pool admission must not wait forever");
        assert!(
            is_database_operation_timeout(&error),
            "unexpected: {error:#}"
        );
        assert!(error.to_string().contains("waiting for connection"));
        drop((first, second));
    });

    let _ = std::fs::remove_file(tmp);
}

#[test]
fn timed_out_connection_is_discarded_before_pool_reuse() {
    let tmp = tmp_db_path("discard_timed_out_connection");
    let db = Db::open(&tmp, 2).unwrap();

    let error = tokio_test_block_on(db.with_conn(|_| {
        Err::<(), _>(database_operation_timed_out(
            "test operation",
            Duration::from_millis(1),
        ))
    }))
    .expect_err("simulated operation must time out");
    assert!(is_database_operation_timeout(&error));
    assert_eq!(
        db.connections.idle.lock().unwrap().len(),
        1,
        "timed-out connection must not be returned to the idle pool"
    );

    let first = ConnectionLease::checkout(Arc::clone(&db.connections)).unwrap();
    let replacement = ConnectionLease::checkout(Arc::clone(&db.connections)).unwrap();
    drop((first, replacement));
    assert_eq!(db.connections.idle.lock().unwrap().len(), 2);

    let _ = std::fs::remove_file(tmp);
}

#[test]
fn failed_remote_operation_discards_connection_before_reuse() {
    let tmp = tmp_db_path("discard_failed_remote_connection");
    let mut db = Db::open(&tmp, 2).unwrap();
    Arc::get_mut(&mut db.connections).unwrap().remote = true;

    let error = tokio_test_block_on(
        db.with_conn(|_| Err::<(), _>(anyhow::anyhow!("simulated transport failure"))),
    )
    .expect_err("simulated remote operation must fail");
    assert!(error.to_string().contains("simulated transport failure"));
    assert_eq!(
        db.connections.idle.lock().unwrap().len(),
        1,
        "failed remote connection must not be returned to the idle pool"
    );

    let _ = std::fs::remove_file(tmp);
}

#[test]
fn cancelled_caller_does_not_release_blocking_task_permit() {
    let tmp = tmp_db_path("cancelled_permit");
    let db = Db::open(&tmp, 2).unwrap();

    tokio_test_block_on(async {
        let worker_db = db.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            worker_db
                .with_conn(move |_conn| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });

        started_rx.await.unwrap();
        assert_eq!(db.permits.available_permits(), 1);
        assert_eq!(db.connections.idle.lock().unwrap().len(), 1);

        task.abort();
        tokio::task::yield_now().await;
        assert_eq!(
            db.permits.available_permits(),
            1,
            "aborting the async caller must not release blocking work's permit"
        );
        assert_eq!(
            db.connections.idle.lock().unwrap().len(),
            1,
            "aborting the async caller must not return an in-use connection"
        );

        release_tx.send(()).unwrap();
        for _ in 0..200 {
            if db.permits.available_permits() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(db.permits.available_permits(), 2);
        assert_eq!(db.connections.idle.lock().unwrap().len(), 2);
    });

    cleanup_db(&tmp);
}

#[test]
fn seeds_globals_in_memory() {
    let tmp = tmp_db_path("seeds_globals");
    let db = Db::open(&tmp, 2).unwrap();
    let parts = db
        .with_conn_sync(|conn| {
            // directly inspect rows
            let rules: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM rules WHERE guild_id = ?1",
                    params![GLOBAL_SCOPE],
                    |r| Ok(r.get(0)?),
                )
                .unwrap();
            let commands: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM commands WHERE guild_id = ?1",
                    params![GLOBAL_SCOPE],
                    |r| Ok(r.get(0)?),
                )
                .unwrap();
            let templates: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM templates WHERE guild_id = ?1",
                    params![GLOBAL_SCOPE],
                    |r| Ok(r.get(0)?),
                )
                .unwrap();
            let badwords: i64 = conn
                .query_row("SELECT COUNT(*) FROM badwords", (), |r| Ok(r.get(0)?))
                .unwrap();
            Ok((rules, commands, templates, badwords))
        })
        .unwrap();
    assert_eq!(
        parts.0,
        crate::types::RULE_IDS.len() as i64,
        "all 7 rules seeded"
    );
    assert!(parts.1 >= 20, "built-in commands seeded");
    assert_eq!(
        parts.2,
        crate::types::EVENT_TYPES.len() as i64,
        "all event templates seeded"
    );
    assert_eq!(parts.3, 0, "no badwords by default");
    cleanup_db(&tmp);
}

#[test]
fn config_parts_loads_every_section() {
    // Regression: load_config_parts must succeed for a real guild and
    // return rules, commands, templates AND log channels (these tables
    // use non-`data` value columns).
    let tmp = tmp_db_path("config_parts_every_section");
    let db = Db::open(&tmp, 2).unwrap();
    let guild = 271083998816632834i64;
    let parts = tokio_test_block_on(crate::db::load_config_parts(&db, guild));
    let parts = parts.unwrap();
    assert_eq!(parts.rules.len(), crate::types::RULE_IDS.len(), "all rules");
    assert!(
        parts.commands.len() >= 20,
        "commands present: {}",
        parts.commands.len()
    );
    assert_eq!(
        parts.templates.len(),
        crate::types::EVENT_TYPES.len(),
        "all templates"
    );
    assert!(parts.templates.contains_key("automod"));
    assert!(parts.log_channels.is_empty(), "no log channels by default");
    cleanup_db(&tmp);
}

#[test]
fn migration_archives_legacy_global_log_channels() {
    let tmp = tmp_db_path("archive_global_logchannels");
    let db = Db::open(&tmp, 2).unwrap();
    db.with_conn_sync(|conn| {
        conn.execute(
            "INSERT INTO log_channels (guild_id, event_type, channel_id) VALUES (0, 'automod', 12345)",
            (),
        )?;
        // Simulate an installation that had reached migration 0002 before
        // the per-guild-only log-channel migration was released.
        conn.execute_batch("DROP TABLE nickname_reconcile; DROP TABLE mutation_receipts; DROP TABLE bot_actions; DROP TABLE guild_enrollments; DELETE FROM meta WHERE key='action_request_floor';")?;
        conn.pragma_update(None, "user_version", 2)?;
        migrate_conn(conn)?;

        let active: i64 = conn.query_row(
            "SELECT COUNT(*) FROM log_channels WHERE guild_id = 0",
            (),
            |r| Ok(r.get(0)?),
        )?;
        let archived: (String, i64) = conn.query_row(
            "SELECT event_type, channel_id FROM legacy_global_log_channels",
            (),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(active, 0);
        assert_eq!(archived, ("automod".into(), 12345));
        Ok(())
    })
    .unwrap();
    cleanup_db(&tmp);
}

fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

fn tmp_db_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard_{label}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cleanup_db(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn config_version_bumps_on_settings_write() {
    let tmp = tmp_db_path("cfgver");
    let db = Db::open(&tmp, 2).unwrap();
    let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    assert!(v0 > 0, "initial seed publishes its config version");
    tokio_test_block_on(crate::db::upsert_settings(
        &db,
        1,
        &GuildSettings::default(),
    ))
    .unwrap();
    let v1 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    assert_eq!(v1, v0 + 1);
    // A remote database must never be opened without credentials.
    let err = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: "libsql://example.invalid".into(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .err()
    .expect("remote URL without a token should fail");
    assert!(
        err.to_string().contains("JMODGUARD_DB_TOKEN is required"),
        "unexpected: {err:#}"
    );
    let err = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: "configured-token".into(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .err()
    .expect("remote token without a URL should fail instead of opening local SQLite");
    assert!(
        err.to_string().contains("JMODGUARD_DB_URL is required"),
        "unexpected: {err:#}"
    );
    cleanup_db(&tmp);
}

#[test]
fn api_open_refuses_unmigrated_db() {
    let tmp = tmp_db_path("unmigrated");
    Connection::open(&tmp).unwrap();
    let err = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .err()
    .expect("unmigrated open should fail");
    let msg = err.to_string();
    assert!(msg.contains("schema is behind"), "unexpected error: {msg}");
    assert!(
        crate::db::is_schema_not_ready(&err),
        "unmigrated API open must be retryable SchemaNotReady"
    );
    cleanup_db(&tmp);
}

#[test]
fn api_open_retries_until_bot_migrates() {
    let tmp = tmp_db_path("api_retry_migrate");
    Connection::open(&tmp).unwrap();
    let api_opts = DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    };
    let bot_opts = DbOpenOptions {
        migrate: true,
        ..api_opts.clone()
    };
    // Prove unreadiness, then retry on a worker while this thread migrates.
    // Putting migrate on a background thread (with or without `sleep`) lets
    // cargo's parallel scheduler starve it until the API budget expires — that
    // failed PR CI after the park() fixture fix.
    let before = Db::open_with(api_opts.clone())
        .err()
        .expect("API must not open before bot migrate");
    assert!(
        is_schema_not_ready(&before),
        "unexpected error before migrate: {before:#}"
    );
    let api = std::thread::spawn(move || {
        tokio_test_block_on(Db::open_with_until_ready(
            api_opts,
            120,
            std::time::Duration::from_millis(50),
        ))
    });
    Db::open_with(bot_opts).expect("bot migrate must succeed");
    api.join()
        .expect("API retry thread panicked")
        .expect("API must succeed once bot migrations finish");
    cleanup_db(&tmp);
}

#[test]
fn api_waits_for_completed_bot_bootstrap() {
    let tmp = tmp_db_path("api_bootstrap_marker");
    let mut conn = Connection::open(&tmp).unwrap();
    migrate_conn(&mut conn).unwrap();
    write_terminal_defaults_meta(&conn, &GuildSettings::default()).unwrap();

    let api_opts = DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    };
    let err = Db::open_with(api_opts.clone())
        .err()
        .expect("API must not open before default rows are seeded");
    assert!(is_schema_not_ready(&err), "unexpected error: {err:#}");
    assert!(
        err.to_string().contains("bootstrap is incomplete"),
        "unexpected error: {err:#}"
    );

    Db::open_with(DbOpenOptions {
        migrate: true,
        ..api_opts.clone()
    })
    .expect("bot bootstrap must complete");
    Db::open_with(api_opts).expect("API must open after bot bootstrap completes");
    cleanup_db(&tmp);
}

#[test]
fn api_open_retries_while_bot_holds_sqlite_lock() {
    let tmp = tmp_db_path("api_retry_locked");
    let db = Db::open(&tmp, 2).unwrap();
    drop(db);

    let lock_path = tmp.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let conn = Connection::open(lock_path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
        locked_tx.send(()).unwrap();
        // Hold long enough for the API open to enter SQLite's busy wait.
        // `PRAGMA busy_timeout=5000` is much larger, so this cannot exhaust
        // the retry budget the way a spurious `park()` wakeup could.
        std::thread::sleep(std::time::Duration::from_millis(100));
        conn.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();

    tokio_test_block_on(Db::open_with_until_ready(
        DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        },
        10,
        std::time::Duration::from_millis(25),
    ))
    .expect("API must wait for a transient bot bootstrap lock");
    holder.join().unwrap();
    cleanup_db(&tmp);
}

#[test]
fn api_open_until_ready_times_out_without_bot() {
    let tmp = tmp_db_path("api_retry_timeout");
    Connection::open(&tmp).unwrap();
    let err = tokio_test_block_on(Db::open_with_until_ready(
        DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 7,
        },
        2,
        std::time::Duration::from_millis(1),
    ))
    .err()
    .expect("must not succeed without bot migrate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("timed out retrying database open"),
        "unexpected error: {msg}"
    );
    assert!(
        crate::db::is_schema_not_ready(&err),
        "timeout must still chain SchemaNotReady"
    );
    cleanup_db(&tmp);
}

#[test]
fn migrate_open_until_ready_timeout_is_not_an_api_wait() {
    let tmp = tmp_db_path("migrate_retry_timeout");
    let lock_path = tmp.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    // `thread::park()` can return spuriously; a COMMIT then lets migrate-open
    // succeed and this assertion fails. Hold the exclusive lock until the
    // timeout path has been observed (sender drop unblocks `recv`).
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let conn = Connection::open(lock_path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
        locked_tx.send(()).unwrap();
        let _ = release_rx.recv();
        conn.execute_batch("COMMIT").unwrap();
    });
    locked_rx.recv().unwrap();
    let err = tokio_test_block_on(Db::open_with_until_ready(
        DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "!".into(),
            retention_days: 7,
        },
        2,
        std::time::Duration::from_millis(1),
    ))
    .err()
    .expect("exclusive lock must exhaust the retry budget");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("timed out retrying database open"),
        "unexpected error: {msg}"
    );
    let lower = msg.to_ascii_lowercase();
    assert!(
        lower.contains("database is locked") || lower.contains("database is busy"),
        "timeout must chain the exclusive-lock failure, not skip open: {msg}"
    );
    assert!(
        !msg.contains("waiting for jmodguard-bot"),
        "bot migrate-open must not report an API wait: {msg}"
    );
    drop(release_tx);
    let _ = holder.join();
    cleanup_db(&tmp);
}

#[test]
fn startup_retries_transient_remote_errors_but_not_auth() {
    let timeout = anyhow::anyhow!("remote database connect timed out after 75s");
    assert!(crate::db::is_startup_retryable(&timeout, true));
    assert!(
        !crate::db::is_startup_retryable(&timeout, false),
        "local SQLite must not treat transport timeouts as a migrate race"
    );
    let refused = anyhow::anyhow!("tcp connect error: connection refused");
    assert!(crate::db::is_startup_retryable(&refused, true));
    // libSQL Hrana: `HranaError::Api("status={}, body={}")` — not reqwest's
    // `status code: N`. 502/503 are Bunny wake; 401/403 are credentials.
    let waking = anyhow::anyhow!("Hrana: `api error: `status=503 Service Unavailable, body=``");
    assert!(crate::db::is_startup_retryable(&waking, true));
    let gateway = anyhow::anyhow!("Hrana: `api error: `status=502 Bad Gateway, body=``");
    assert!(crate::db::is_startup_retryable(&gateway, true));
    let auth = anyhow::anyhow!("Hrana: `api error: `status=401 Unauthorized, body=``");
    assert!(!crate::db::is_startup_retryable(&auth, true));
    let forbidden = anyhow::anyhow!("Hrana: `api error: `status=403 Forbidden, body=``");
    assert!(!crate::db::is_startup_retryable(&forbidden, true));
    let readonly = anyhow::anyhow!("attempt to write a readonly database");
    assert!(
        !crate::db::is_startup_retryable(&readonly, true),
        "read-only tokens must fail immediately, not burn the remote retry window"
    );
    let not_authorized = anyhow::anyhow!("Hrana: not authorized");
    assert!(
        !crate::db::is_startup_retryable(&not_authorized, true),
        "libSQL 'not authorized' is a permanent token failure, not a 401 Unauthorized substring"
    );
    let not_authorized_try_again = anyhow::anyhow!("not authorized, try again");
    assert!(
        !crate::db::is_startup_retryable(&not_authorized_try_again, true),
        "authz wording must win over a transient 'try again' needle"
    );
    let permission_reset = anyhow::anyhow!("permission denied: connection reset");
    assert!(
        !crate::db::is_startup_retryable(&permission_reset, true),
        "permission denied must not burn the remote retry window as a transport reset"
    );
    let annotated = anyhow::Error::new(crate::db::RemoteTokenCannotWrite(
        "not authorized, try again".into(),
    ));
    assert!(
        !crate::db::is_startup_retryable(&annotated, true),
        "write-probe annotation must stay fail-closed even when the cause looks transient"
    );
    let handshake = anyhow::anyhow!("tls handshake: client authentication timed out");
    assert!(
        crate::db::is_startup_retryable(&handshake, true),
        "TLS wording that contains 'authentication' must still retry"
    );
    let schema = anyhow::anyhow!(crate::db::SchemaNotReady("schema is behind".into()));
    assert!(crate::db::is_startup_retryable(&schema, false));
    assert!(crate::db::is_startup_retryable(&schema, true));
}

#[test]
fn ensure_api_writes_allowed_fails_closed_on_query_only() {
    let tmp = tmp_db_path("api_write_probe_query_only");
    let _bot = Db::open(&tmp, 2).expect("bot migrate-open");
    let conn = Connection::open(&tmp).unwrap();
    conn.execute_batch("PRAGMA query_only=ON;").unwrap();
    let err = ensure_api_writes_allowed(&conn)
        .err()
        .expect("query_only connection must fail the API write probe");
    let msg = err.to_string();
    assert!(msg.contains("cannot write"), "unexpected: {msg}");
    assert!(
        err.downcast_ref::<crate::db::RemoteTokenCannotWrite>()
            .is_some(),
        "write-probe denial must keep a typed non-retryable error, not only help text: {msg}"
    );
    assert!(
        !crate::db::is_startup_retryable(&err, true),
        "write-probe denial must not burn the remote retry window"
    );
    cleanup_db(&tmp);
}

#[test]
fn migrate_open_fences_only_when_bootstrap_is_incomplete() {
    let tmp = tmp_db_path("migrate_fence");
    let empty = Connection::open(&tmp).unwrap();
    assert!(
        migrate_open_needs_api_fence(&empty).unwrap(),
        "an empty database must fence the API until the bot finishes bootstrap"
    );
    drop(empty);

    let bot = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .expect("bot migrate-open");
    bot.with_conn_sync(|conn| {
        assert!(
            !migrate_open_needs_api_fence(conn)?,
            "a finished bootstrap must not hide readiness on reconnect"
        );
        Ok(())
    })
    .unwrap();

    Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .expect("API must open while the bot reconnects a ready database");

    bot.with_conn_sync(|conn| {
        clear_bootstrap_complete(conn)?;
        assert!(
            migrate_open_needs_api_fence(conn)?,
            "a missing marker must fence the API during a real bootstrap"
        );
        Ok(())
    })
    .unwrap();
    let err = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .err()
    .expect("API must wait while bootstrap is incomplete");
    assert!(crate::db::is_schema_not_ready(&err), "unexpected: {err:#}");
    cleanup_db(&tmp);
}

#[test]
fn delete_badword_rejects_other_guild_idor() {
    let tmp = tmp_db_path("badword_idor");
    let db = Db::open(&tmp, 2).unwrap();
    let id_a = tokio_test_block_on(crate::db::add_badword(&db, 11, "spam", None)).unwrap();
    let id_b = tokio_test_block_on(crate::db::add_badword(&db, 22, "scam", None)).unwrap();
    let err = tokio_test_block_on(crate::db::delete_badword(&db, id_a, 22))
        .err()
        .expect("cross-guild delete should fail");
    assert!(err.to_string().contains("not found"), "unexpected: {err:#}");
    assert!(
        !err.to_string().contains("for this guild"),
        "must not leak guild-scope detail: {err:#}"
    );
    tokio_test_block_on(crate::db::delete_badword(&db, id_b, 22)).unwrap();
    // Global rows only delete when the exact GLOBAL_SCOPE is passed.
    let id_g =
        tokio_test_block_on(crate::db::add_badword(&db, GLOBAL_SCOPE, "global", None)).unwrap();
    let err = tokio_test_block_on(crate::db::delete_badword(&db, id_g, 11))
        .err()
        .expect("guild context must not delete global via exact match");
    assert!(err.to_string().contains("not found"), "unexpected: {err:#}");
    assert!(
        !err.to_string().contains("for this guild"),
        "must not leak guild-scope detail: {err:#}"
    );
    tokio_test_block_on(crate::db::delete_badword(&db, id_g, GLOBAL_SCOPE)).unwrap();
    cleanup_db(&tmp);
}

#[test]
fn add_badword_rejects_pathological_pattern() {
    let tmp = tmp_db_path("badword_patho");
    let db = Db::open(&tmp, 2).unwrap();
    let err = tokio_test_block_on(crate::db::add_badword(&db, 11, r"\w{100000}", None))
        .err()
        .expect("pathological pattern must be rejected at persist");
    assert!(
        err.to_string().contains("invalid regex") || err.to_string().contains("size"),
        "unexpected: {err:#}"
    );
    let huge = "a".repeat(crate::normalize::USER_REGEX_MAX_PATTERN_BYTES + 1);
    let err = tokio_test_block_on(crate::db::add_badword(&db, 11, &huge, None))
        .err()
        .expect("oversized pattern must be rejected");
    assert!(err.to_string().contains("too long"), "unexpected: {err:#}");
    cleanup_db(&tmp);
}

#[test]
fn copy_guild_config_skips_invalid_badwords() {
    let tmp = tmp_db_path("copy_badword");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::add_badword(&db, 11, "scam", None)).unwrap();
    // Legacy row inserted past the gate (raw SQL) — copy must not propagate it.
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
            params![r"\w{100000}"],
        )?;
        Ok(())
    }))
    .unwrap();
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["badwords".into()],
    ))
    .unwrap();
    assert_eq!(result.copied, vec!["badwords"]);
    assert_eq!(
        result.skipped_badwords, 1,
        "pathological pattern must be counted"
    );
    let words = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
    assert_eq!(words.len(), 1, "only valid pattern should copy: {words:?}");
    assert_eq!(words[0].pattern, "scam");
    let view = words[0].with_status();
    assert!(view.valid);
    cleanup_db(&tmp);
}

#[test]
fn copy_badwords_refuses_all_invalid_and_preserves_destination() {
    // Root-cause regression: DELETE-then-filter wiped dest when source had
    // only unenforceable patterns.
    let tmp = tmp_db_path("copy_wipe");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::add_badword(&db, 22, "keep-me", None)).unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
            params![r"\w{100000}"],
        )?;
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
            params!["a".repeat(crate::normalize::USER_REGEX_MAX_PATTERN_BYTES + 1)],
        )?;
        Ok(())
    }))
    .unwrap();

    let err = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["badwords".into()],
    ))
    .err()
    .expect("all-invalid source must refuse copy");
    assert!(
        err.to_string().contains("refused")
            || err.to_string().contains("destination left unchanged"),
        "unexpected: {err:#}"
    );

    let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
    assert_eq!(dest.len(), 1, "destination must be preserved: {dest:?}");
    assert_eq!(dest[0].pattern, "keep-me");
    cleanup_db(&tmp);
}

#[test]
fn copy_badwords_empty_source_clears_destination() {
    // Intentional empty replace — source has zero rows, dest should clear.
    let tmp = tmp_db_path("copy_empty");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::add_badword(&db, 22, "old", None)).unwrap();
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["badwords".into()],
    ))
    .unwrap();
    assert_eq!(result.copied, vec!["badwords"]);
    assert_eq!(result.skipped_badwords, 0);
    let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
    assert!(dest.is_empty(), "empty source must clear dest: {dest:?}");
    cleanup_db(&tmp);
}

#[test]
fn copy_all_rolls_back_when_badwords_refuse() {
    // Atomic transaction: refused badwords must not leave partial section writes.
    let tmp = tmp_db_path("copy_atomic");
    let db = Db::open(&tmp, 2).unwrap();
    let mut settings = GuildSettings::default();
    settings.prefix = "!src".into();
    tokio_test_block_on(crate::db::upsert_settings(&db, 11, &settings)).unwrap();
    tokio_test_block_on(crate::db::add_badword(&db, 22, "keep", None)).unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
            params![r"\w{100000}"],
        )?;
        Ok(())
    }))
    .unwrap();

    let err = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["all".into()],
    ))
    .err()
    .expect("all-invalid badwords must refuse entire copy");
    assert!(err.to_string().contains("refused"), "unexpected: {err:#}");

    let dest_settings = tokio_test_block_on(crate::db::get_settings(&db, 22)).unwrap();
    assert_ne!(
        dest_settings.prefix, "!src",
        "settings must roll back when badwords refuse"
    );
    let dest = tokio_test_block_on(crate::db::list_badwords(&db, 22)).unwrap();
    assert_eq!(dest[0].pattern, "keep");
    cleanup_db(&tmp);
}

#[test]
fn guild_stats_are_scoped_not_global() {
    let tmp = tmp_db_path("guild_stats");
    let db = Db::open(&tmp, 2).unwrap();
    let g1 = 111i64;
    let g2 = 222i64;
    tokio_test_block_on(crate::db::add_warn(&db, g1, 1, 9, "spam")).unwrap();
    tokio_test_block_on(crate::db::add_warn(&db, g2, 3, 9, "spam")).unwrap();
    tokio_test_block_on(crate::db::add_warn(&db, g2, 4, 9, "spam")).unwrap();
    tokio_test_block_on(crate::db::record_ban(&db, g2, 4, Some(9), "spam")).unwrap();
    // No historical member/message tables are needed for guild stats.
    db.with_conn_sync(|conn| {
        conn.execute_batch("DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS members;")?;
        Ok(())
    })
    .unwrap();
    let s1 = tokio_test_block_on(crate::db::guild_stats(&db, g1)).unwrap();
    let s2 = tokio_test_block_on(crate::db::guild_stats(&db, g2)).unwrap();
    assert_eq!(s1.warns, 1);
    assert_eq!(s2.warns, 2);
    assert_eq!(s1.bans, 0);
    assert_eq!(s2.bans, 1);
    let wire = serde_json::to_value(s1).unwrap();
    assert!(wire.get("messages").is_none());
    assert!(wire.get("members").is_none());
    cleanup_db(&tmp);
}

#[test]
fn moderation_stats_exclude_routine_and_unclassified_history() {
    let tmp = tmp_db_path("moderation_stats_history");
    let db = Db::open(&tmp, 2).unwrap();
    let baseline = tokio_test_block_on(get_config_version(&db)).unwrap();
    let routine = [
        ("system", serde_json::json!({})),
        ("message_edit", serde_json::json!({})),
        ("message_delete", serde_json::json!({})),
        ("automod", serde_json::json!({"action":"log"})),
        ("automod", serde_json::json!({"action":"delete"})),
        (
            "automod",
            serde_json::json!({"action":"ban", "dry_run":true}),
        ),
        (
            "plan_executed",
            serde_json::json!({"command":"old", "steps":1}),
        ),
        ("plan_executed", serde_json::json!({"moderation":false})),
        ("plan_executed", serde_json::json!({"moderation":"true"})),
    ];
    for (event, data) in &routine {
        tokio_test_block_on(insert_event(&db, 11, event, None, None, data)).unwrap();
    }
    // Transitional rows can contain malformed JSON; they must not break stats.
    db.with_conn_sync(|conn| {
        conn.execute_batch(
            "INSERT INTO events (guild_id,event_type,data,created_at)
             VALUES (11,'automod','invalid',''), (11,'plan_executed',NULL,'');",
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(tokio_test_block_on(guild_stats(&db, 11)).unwrap().events, 0);
    let retained = [
        ("mod_action", serde_json::json!({"action":"warn"})),
        ("timeout_add", serde_json::json!({})),
        ("timeout_remove", serde_json::json!({})),
        ("member_ban", serde_json::json!({})),
        ("member_unban", serde_json::json!({})),
        ("member_kick", serde_json::json!({})),
        ("automod", serde_json::json!({"action":"warn"})),
        (
            "automod",
            serde_json::json!({"action":"timeout", "dry_run":false}),
        ),
        ("automod", serde_json::json!({"action":"kick"})),
        ("automod", serde_json::json!({"action":"ban"})),
        (
            "plan_executed",
            serde_json::json!({"command":"new", "steps":1, "moderation":true}),
        ),
    ];
    for guild in [11, 22] {
        for (event, data) in &retained {
            tokio_test_block_on(insert_event(&db, guild, event, None, None, data)).unwrap();
        }
    }
    assert_eq!(
        tokio_test_block_on(guild_stats(&db, 11)).unwrap().events,
        retained.len() as i64
    );
    assert_eq!(
        tokio_test_block_on(guild_stats(&db, 22)).unwrap().events,
        retained.len() as i64
    );
    // Counting does not erase or hide the ambiguous/routine legacy audit rows.
    assert_eq!(
        tokio_test_block_on(query_events(&db, 11, None, None, 200))
            .unwrap()
            .len(),
        routine.len() + 2 + retained.len()
    );
    assert_eq!(
        tokio_test_block_on(get_config_version(&db)).unwrap(),
        baseline
    );
    cleanup_db(&tmp);
}

#[test]
fn dual_process_local_share_config_version() {
    // Bot migrates; API opens migrate=false; writer bump visible to both.
    let tmp = tmp_db_path("dual");
    let bot = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    let api = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    let baseline = tokio_test_block_on(crate::db::get_config_version(&bot)).unwrap();
    tokio_test_block_on(crate::db::upsert_settings(
        &api,
        42,
        &GuildSettings::default(),
    ))
    .unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_config_version(&bot)).unwrap(),
        baseline + 1
    );
    assert_eq!(
        tokio_test_block_on(crate::db::get_config_version(&api)).unwrap(),
        baseline + 1
    );
    cleanup_db(&tmp);
}

#[test]
fn config_parts_loads_effective_rules() {
    let tmp = tmp_db_path("config_parts_effective_rules");
    let db = Db::open(&tmp, 2).unwrap();
    let parts = db.with_conn_sync(|_| Ok(())).unwrap();
    let _ = parts;
    // rule id keys must exist and match RULE_IDS
    let ids: Vec<String> = db
        .with_conn_sync(|conn| {
            let mut stmt =
                conn.prepare("SELECT rule_id FROM rules WHERE guild_id = 0 ORDER BY rule_id")?;
            let rows = stmt.query_map((), |r| Ok(r.get::<String>(0)?))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .unwrap();
    let mut expected: Vec<String> = crate::types::RULE_IDS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut got = ids;
    got.sort();
    expected.sort();
    assert_eq!(got, expected);
    cleanup_db(&tmp);
}

#[test]
fn slash_registry_version_bumps_only_on_command_writes() {
    let tmp = tmp_db_path("slashver");
    let db = Db::open(&tmp, 2).unwrap();
    let s0 = tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap();
    assert!(
        s0 > 0,
        "initial command seed publishes its registry version"
    );

    tokio_test_block_on(crate::db::upsert_settings(
        &db,
        1,
        &GuildSettings::default(),
    ))
    .unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
        s0,
        "settings must not bump slash_registry_version"
    );

    let cmd = crate::defaults::default_commands()
        .into_iter()
        .next()
        .expect("defaults include commands");
    tokio_test_block_on(crate::db::upsert_command(&db, 1, &cmd.name, &cmd)).unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
        s0 + 1,
        "command upsert must bump slash_registry_version"
    );
    assert!(
        tokio_test_block_on(crate::db::get_config_version(&db)).unwrap() >= 2,
        "command upsert still bumps config_version"
    );
    let (loaded_version, _loaded_revision, loaded_commands) =
        tokio_test_block_on(crate::db::load_slash_commands_with_version(&db, 1)).unwrap();
    assert_eq!(loaded_version, s0 + 1);
    assert_eq!(
        loaded_commands.get(&cmd.name).map(|loaded| &loaded.name),
        Some(&cmd.name)
    );

    tokio_test_block_on(crate::db::delete_command(&db, 1, &cmd.name)).unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
        s0 + 2
    );
    cleanup_db(&tmp);
}

#[test]
fn event_row_snowflakes_serialize_as_strings() {
    let row = EventRow {
        id: 1,
        guild_id: 1329331947876479006,
        event_type: "kick".into(),
        user_id: Some(1329331947876479007),
        actor_id: None,
        data: None,
        created_at: "t".into(),
    };
    let v = serde_json::to_value(&row).unwrap();
    assert_eq!(v["guild_id"], "1329331947876479006");
    assert_eq!(v["user_id"], "1329331947876479007");
    assert!(v["actor_id"].is_null());
}

#[test]
fn insert_event_query_events_roundtrips_data_snowflakes_as_strings() {
    let tmp = tmp_db_path("event_data_sf");
    let db = Db::open(&tmp, 2).unwrap();
    let guild = 11i64;
    let snow = 1329331947876479006u64;
    tokio_test_block_on(crate::db::insert_event(
        &db,
        guild,
        "message_delete",
        Some(snow as i64),
        None,
        &serde_json::json!({
            "channel_id": snow,
            "message_id": 99,
            "nested": { "id": snow + 1 },
        }),
    ))
    .unwrap();

    let rows = tokio_test_block_on(crate::db::query_events(&db, guild, None, None, 10)).unwrap();
    assert_eq!(rows.len(), 1);
    let data: serde_json::Value = serde_json::from_str(rows[0].data.as_deref().unwrap()).unwrap();
    assert_eq!(data["channel_id"], "1329331947876479006");
    assert_eq!(data["message_id"], 99);
    assert_eq!(data["nested"]["id"], "1329331947876479007");
    // Top-level EventRow IDs are also strings on the wire.
    let wire = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(wire["user_id"], "1329331947876479006");
    cleanup_db(&tmp);
}

#[test]
fn query_events_heals_legacy_numeric_data_blobs() {
    let tmp = tmp_db_path("event_legacy");
    let db = Db::open(&tmp, 2).unwrap();
    let snow = 1329331947876479006i64;
    // Simulate a pre-fix row that stored snowflakes as JSON numbers.
    tokio_test_block_on(db.with_conn(move |conn| {
        conn.execute(
            "INSERT INTO events (guild_id, event_type, user_id, actor_id, data, created_at)
             VALUES (1, 'message_edit', NULL, NULL, ?1, 't')",
            params![format!(r#"{{"channel_id":{snow},"message_id":1}}"#)],
        )?;
        Ok(())
    }))
    .unwrap();
    let rows = tokio_test_block_on(crate::db::query_events(&db, 1, None, None, 10)).unwrap();
    let data: serde_json::Value = serde_json::from_str(rows[0].data.as_deref().unwrap()).unwrap();
    assert_eq!(data["channel_id"], "1329331947876479006");
    assert_eq!(data["message_id"], 1);
    cleanup_db(&tmp);
}

#[test]
fn guild_without_settings_row_inherits_global() {
    let tmp = tmp_db_path("settings_inherit");
    let db = Db::open(&tmp, 2).unwrap();
    let guild = 4242i64;
    // No guild_settings row — global prefix must apply.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("?".into())),
        None,
        None,
    ))
    .unwrap();
    let effective = tokio_test_block_on(crate::db::get_settings(&db, guild)).unwrap();
    assert_eq!(effective.prefix, "?");
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, guild))
            .unwrap()
            .is_none()
    );
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_only_overrides_changed_fields() {
    let tmp = tmp_db_path("settings_patch");
    let db = Db::open(&tmp, 2).unwrap();
    let guild = 77i64;
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("g".into())),
        Some(Some("GlobalBot".into())),
        Some(Some(14)),
    ))
    .unwrap();
    // Guild sets only prefix; nickname/retention stay inherited.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        guild,
        Some(Some("p".into())),
        None,
        None,
    ))
    .unwrap();
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, guild))
        .unwrap()
        .expect("guild override row");
    assert_eq!(ov.prefix.as_deref(), Some("p"));
    assert!(ov.bot_name.is_none());
    assert!(ov.retention_days.is_none());
    let effective = tokio_test_block_on(crate::db::get_settings(&db, guild)).unwrap();
    assert_eq!(effective.prefix, "p");
    assert_eq!(effective.bot_name, "");
    assert_eq!(effective.retention_days, 14);

    // Saving a value equal to the parent clears that override field.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        guild,
        Some(Some("g".into())),
        None,
        None,
    ))
    .unwrap();
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, guild))
            .unwrap()
            .is_none(),
        "override row must be deleted when empty"
    );
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_null_clears_field_override() {
    let tmp = tmp_db_path("settings_clear_field");
    let db = Db::open(&tmp, 2).unwrap();
    let guild = 88i64;
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        None,
        None,
        Some(Some(30)),
    ))
    .unwrap();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        guild,
        None,
        None,
        Some(Some(14)),
    ))
    .unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&db, guild))
            .unwrap()
            .retention_days,
        14
    );
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        guild,
        None,
        None,
        Some(None),
    ))
    .unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings_override(&db, guild))
            .unwrap()
            .and_then(|o| o.retention_days),
        None,
        "null patch must clear retention override"
    );
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&db, guild))
            .unwrap()
            .retention_days,
        30,
        "must inherit global retention after clear"
    );
    cleanup_db(&tmp);
}

#[test]
fn heal_normalizes_legacy_blobs_and_preserves_intentional_overrides() {
    let tmp = tmp_db_path("settings_heal");
    // Pre-fix DB shapes:
    // - guild 99: ambiguous full hardcoded-default blob (could be an
    //   ensure_guild seed or an intentional operator override)
    // - guild 88: intentional *partial* prefix override "!" (new durable shape)
    // - guild 77: old get_settings→upsert full effective blob with only
    //   retention intentionally different
    {
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(crate::db::patch_settings(
            &db,
            GLOBAL_SCOPE,
            Some(Some("?".into())),
            Some(Some(String::new())),
            Some(Some(7)),
        ))
        .unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (99, ?1)",
                params![serde_json::to_string(&GuildSettings::default())?],
            )?;
            // Partial override — the durable intentional shape.
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (88, ?1)",
                params![serde_json::to_string(&GuildSettingsOverride {
                    prefix: Some("!".into()),
                    bot_name: None,
                    retention_days: None,
                })?],
            )?;
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (77, ?1)",
                params![serde_json::to_string(&GuildSettingsOverride {
                    prefix: Some("?".into()),
                    bot_name: Some(String::new()),
                    retention_days: Some(30),
                })?],
            )?;
            Ok(())
        }))
        .unwrap();
    }

    let db = Db::open(&tmp, 2).unwrap();
    // 99: legacy format has no provenance. Preserve the explicit values
    // instead of deleting a possible intentional operator override.
    let ov99 = tokio_test_block_on(crate::db::get_settings_override(&db, 99))
        .unwrap()
        .expect("ambiguous full default blob must be preserved");
    assert_eq!(ov99.prefix.as_deref(), Some("!"));
    assert!(ov99.bot_name.is_none());
    assert!(ov99.retention_days.is_none());
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&db, 99))
            .unwrap()
            .prefix,
        "!"
    );

    // 88: intentional partial ! override preserved
    let ov88 = tokio_test_block_on(crate::db::get_settings_override(&db, 88))
        .unwrap()
        .expect("intentional ! override must survive");
    assert_eq!(ov88.prefix.as_deref(), Some("!"));
    assert!(ov88.bot_name.is_none());
    assert!(ov88.retention_days.is_none());

    // 77: only retention differs → partial override
    let ov77 = tokio_test_block_on(crate::db::get_settings_override(&db, 77))
        .unwrap()
        .expect("retention-only override");
    assert!(ov77.prefix.is_none());
    assert!(ov77.bot_name.is_none());
    assert_eq!(ov77.retention_days, Some(30));

    assert!(
        tokio_test_block_on(crate::db::get_config_version(&db)).unwrap() >= 1,
        "heal must bump config_version when rows change"
    );

    // Pure default blob when global also matches Default → delete.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("!".into())),
        Some(Some(String::new())),
        Some(Some(7)),
    ))
    .unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (66, ?1)",
            params![serde_json::to_string(&GuildSettings::default())?],
        )?;
        Ok(())
    }))
    .unwrap();
    drop(db);
    let db = Db::open(&tmp, 2).unwrap();
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, 66))
            .unwrap()
            .is_none(),
        "default blob equal to global must be removed"
    );
    cleanup_db(&tmp);
}

#[test]
fn heal_normalizes_global_and_clears_corrupt_rows() {
    let tmp = tmp_db_path("settings_heal_global");
    {
        let db = Db::open(&tmp, 2).unwrap();
        // Replace seed with a legacy full hardcoded-default global blob
        // (old writers pinned every field).
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "UPDATE guild_settings SET settings = ?1 WHERE guild_id = 0",
                params![serde_json::to_string(&GuildSettings::default())?],
            )?;
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (55, ?1)",
                params!["{not-json"],
            )?;
            Ok(())
        }))
        .unwrap();
    }

    let db = Db::open(&tmp, 2).unwrap();
    let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
        .unwrap()
        .unwrap_or_default();
    assert!(
        global.is_empty(),
        "legacy full-default global blob must normalize to empty override"
    );
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, 55))
            .unwrap()
            .is_none(),
        "corrupt guild_settings JSON must be cleared"
    );
    // get_settings must succeed after corrupt-row clear.
    let effective = tokio_test_block_on(crate::db::get_settings(&db, 55)).unwrap();
    assert_eq!(effective.prefix, GuildSettings::default().prefix);
    cleanup_db(&tmp);
}

#[test]
fn seed_globals_writes_parent_relative_partial_override() {
    let tmp = tmp_db_path("settings_seed_partial");
    let db = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "?".into(),
        retention_days: 7,
    })
    .unwrap();
    // Seed writes config-vs-hardcoded diffs, then heal normalizes the global
    // row against config terminal defaults — so a prefix that only mirrors
    // config ends up empty in storage and is provided by terminal defaults.
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
        .unwrap()
        .unwrap_or_default();
    assert!(
        ov.prefix.is_none(),
        "prefix equal to config terminal default must not stay pinned in global"
    );
    assert!(
        ov.bot_name.is_none(),
        "seed must not pin empty bot_name as an override"
    );
    assert!(
        ov.retention_days.is_none(),
        "seed retention equal to hardcoded default must be stripped"
    );
    assert_eq!(db.read_terminal_defaults().unwrap().prefix, "?");
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&db, GLOBAL_SCOPE))
            .unwrap()
            .prefix,
        "?"
    );
    cleanup_db(&tmp);
}

#[test]
fn copy_refuses_all_invalid_badwords_before_writes() {
    let tmp = tmp_db_path("preflight_badwords");
    let db = Db::open(&tmp, 2).unwrap();
    // Legacy row past the add gate — same shape that refuses a live copy.
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO badwords (guild_id, pattern, note) VALUES (11, ?1, NULL)",
            params![r"("],
        )?;
        Ok(())
    }))
    .unwrap();
    let err = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["settings".into(), "badwords".into()],
    ))
    .err()
    .expect("copy transaction must refuse");
    assert!(err.to_string().contains("refused"), "unexpected: {err:#}");
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_strips_legacy_materialized_fields() {
    let tmp = tmp_db_path("settings_patch_legacy");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("g".into())),
        Some(Some("GlobalBot".into())),
        Some(Some(14)),
    ))
    .unwrap();
    // Simulate old writer: full effective blob stored as guild row.
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (77, ?1)",
            params![serde_json::to_string(&GuildSettingsOverride {
                prefix: Some("g".into()),
                bot_name: None,
                retention_days: Some(14),
            })?],
        )?;
        Ok(())
    }))
    .unwrap();
    // Patch only retention — must clear inherited prefix/bot_name pins.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        77,
        None,
        None,
        Some(Some(30)),
    ))
    .unwrap();
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 77))
        .unwrap()
        .unwrap();
    assert!(ov.prefix.is_none());
    assert!(ov.bot_name.is_none());
    assert_eq!(ov.retention_days, Some(30));
    cleanup_db(&tmp);
}

#[test]
fn copy_settings_copies_override_not_defaults() {
    let tmp = tmp_db_path("copy_settings_inherit");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("@@".into())),
        None,
        None,
    ))
    .unwrap();
    // Source has no override — dest override must be cleared / absent.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        22,
        Some(Some("old".into())),
        None,
        None,
    ))
    .unwrap();
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["settings".into()],
    ))
    .unwrap();
    assert_eq!(result.copied, vec!["settings"]);
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, 22))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&db, 22))
            .unwrap()
            .prefix,
        "@@"
    );

    // Source with an explicit override copies that override only.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        11,
        Some(Some("src".into())),
        None,
        None,
    ))
    .unwrap();
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["settings".into()],
    ))
    .unwrap();
    assert_eq!(result.copied, vec!["settings"]);
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 22))
        .unwrap()
        .unwrap();
    assert_eq!(ov.prefix.as_deref(), Some("src"));
    assert!(ov.bot_name.is_none());

    // Write-path copy must preserve intentional hardcoded-default pins
    // (same contract as patch_settings), including hardcoded-default pins.
    let d = GuildSettings::default();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("?".into())),
        Some(Some("GlobalBot".into())),
        Some(Some(30)),
    ))
    .unwrap();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        11,
        Some(Some(d.prefix.clone())),
        Some(Some(d.bot_name.clone())),
        Some(Some(d.retention_days)),
    ))
    .unwrap();
    let preview = tokio_test_block_on(crate::db::preview_settings_after_copy(&db, 11, 22)).unwrap();
    assert_eq!(preview.prefix, "!");
    assert_eq!(preview.bot_name, "");
    assert_eq!(preview.retention_days, 7);
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["settings".into()],
    ))
    .unwrap();
    assert_eq!(result.copied, vec!["settings"]);
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 22))
        .unwrap()
        .expect("intentional hardcoded-default pin must copy");
    assert_eq!(ov.prefix.as_deref(), Some("!"));
    assert!(ov.bot_name.is_none());
    assert_eq!(ov.retention_days, Some(7));
    cleanup_db(&tmp);
}

#[test]
fn copy_sections_remove_destination_only_overrides() {
    let tmp = tmp_db_path("copy_sections_replace");
    let db = Db::open(&tmp, 2).unwrap();

    // The source inherits every section from global defaults, while the
    // destination has local rows. A copy must clear those rows rather than
    // leave them silently winning effective resolution.
    let mut rule = crate::defaults::default_rules().into_iter().next().unwrap();
    let rule_id = crate::types::RULE_IDS[0];
    rule.enabled = false;
    tokio_test_block_on(crate::db::upsert_rule(&db, 22, rule_id, &rule)).unwrap();

    let command = crate::defaults::sample_plan_command();
    tokio_test_block_on(crate::db::upsert_command(&db, 22, &command.name, &command)).unwrap();
    tokio_test_block_on(crate::db::upsert_template(
        &db,
        22,
        "member_join",
        "destination-only template",
    ))
    .unwrap();

    tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["rules".into(), "commands".into(), "templates".into()],
    ))
    .unwrap();

    assert!(
        tokio_test_block_on(crate::db::get_rule(&db, 22, rule_id))
            .unwrap()
            .is_none()
    );
    assert!(
        tokio_test_block_on(crate::db::get_command_row(&db, 22, &command.name))
            .unwrap()
            .is_none()
    );
    assert!(
        tokio_test_block_on(crate::db::get_template_row(&db, 22, "member_join"))
            .unwrap()
            .is_none()
    );
    cleanup_db(&tmp);
}

#[test]
fn global_settings_never_persist_a_guild_nickname() {
    let tmp = tmp_db_path("global_settings_no_nickname");
    let db = Db::open(&tmp, 2).unwrap();

    // The shared write path enforces the invariant even when a caller does
    // not go through the Web API.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("?".into())),
        Some(Some("not-global".into())),
        None,
    ))
    .unwrap();
    let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
        .unwrap()
        .unwrap();
    assert_eq!(global.prefix.as_deref(), Some("?"));
    assert!(global.bot_name.is_none());

    // Copy uses the same invariant, so a per-guild nickname cannot regain
    // global scope through this alternate writer.
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        11,
        None,
        Some(Some("guild-only".into())),
        None,
    ))
    .unwrap();
    tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        GLOBAL_SCOPE,
        vec!["settings".into()],
    ))
    .unwrap();
    let global = tokio_test_block_on(crate::db::get_settings_override(&db, GLOBAL_SCOPE))
        .unwrap()
        .unwrap();
    assert!(global.bot_name.is_none());
    cleanup_db(&tmp);
}

#[test]
fn copy_logchannels_skips_cross_guild() {
    let tmp = tmp_db_path("copy_logchannels_skip");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::upsert_log_channel(&db, 11, "member_join", 12345)).unwrap();
    let err = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["logchannels".into()],
    ))
    .err()
    .expect("cross-guild logchannels must refuse");
    assert!(
        err.to_string().contains("server-specific"),
        "unexpected: {err:#}"
    );
    let dest = tokio_test_block_on(crate::db::list_log_channels(&db, 22)).unwrap();
    assert!(dest.is_empty());

    // "all" skips logchannels without failing other sections.
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        22,
        vec!["all".into()],
    ))
    .unwrap();
    assert!(result.skipped_logchannels);
    assert!(!result.copied.contains(&"logchannels"));
    let dest = tokio_test_block_on(crate::db::list_log_channels(&db, 22)).unwrap();
    assert!(dest.is_empty(), "must not copy foreign channel ids");

    // Same guild — logchannels is a no-op, not a false "copied".
    let result = tokio_test_block_on(crate::db::copy_guild_config(
        &db,
        11,
        11,
        vec!["logchannels".into()],
    ))
    .unwrap();
    assert!(
        !result.copied.contains(&"logchannels"),
        "same-guild logchannels must not report copied"
    );
    assert!(!result.skipped_logchannels);
    cleanup_db(&tmp);
}

#[test]
fn log_channels_require_positive_snowflakes() {
    let tmp = tmp_db_path("positive_log_channel");
    let db = Db::open(&tmp, 2).unwrap();
    let err =
        tokio_test_block_on(crate::db::upsert_log_channel(&db, 11, "member_join", -1)).unwrap_err();
    assert!(err.to_string().contains("positive Discord snowflake"));
    assert!(
        tokio_test_block_on(crate::db::list_log_channels(&db, 11))
            .unwrap()
            .is_empty()
    );
    cleanup_db(&tmp);
}

#[test]
fn api_open_leaves_healing_to_bot_authority() {
    let tmp = tmp_db_path("heal_api_open");
    {
        let db = Db::open(&tmp, 2).unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO guild_settings (guild_id, settings) VALUES (55, ?1)",
                params!["{not-json"],
            )?;
            Ok(())
        }))
        .unwrap();
    }

    let db = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    let raw: String = db
        .with_conn_sync(|conn| {
            conn.query_row(
                "SELECT settings FROM guild_settings WHERE guild_id = 55",
                (),
                |row| Ok(row.get(0)?),
            )
        })
        .unwrap();
    assert_eq!(raw, "{not-json");
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_pins_hardcoded_defaults_when_global_differs() {
    // Root cause lock: write-path must NOT clear ensure_guild-shaped values.
    // Intentional "match hardcoded defaults while global differs" must pin.
    let tmp = tmp_db_path("patch_pin_defaults");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some("?".into())),
        Some(Some("GlobalBot".into())),
        Some(Some(30)),
    ))
    .unwrap();
    let d = GuildSettings::default();
    tokio_test_block_on(crate::db::patch_settings(
        &db,
        42,
        Some(Some(d.prefix.clone())),
        Some(Some(d.bot_name.clone())),
        Some(Some(d.retention_days)),
    ))
    .unwrap();
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 42))
        .unwrap()
        .expect("intentional hardcoded-default override must survive write path");
    assert_eq!(ov.prefix.as_deref(), Some("!"));
    assert!(ov.bot_name.is_none());
    assert_eq!(ov.retention_days, Some(7));
    let effective = tokio_test_block_on(crate::db::get_settings(&db, 42)).unwrap();
    assert_eq!(effective.prefix, "!");
    assert_eq!(effective.bot_name, "");
    assert_eq!(effective.retention_days, 7);
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_trims_prefix_whitespace() {
    let tmp = tmp_db_path("patch_prefix_trim");
    let db = Db::open(&tmp, 2).unwrap();
    let effective = tokio_test_block_on(crate::db::patch_settings(
        &db,
        7,
        Some(Some("  ?  ".into())),
        None,
        None,
    ))
    .unwrap();
    assert_eq!(effective.prefix, "?");
    let ov = tokio_test_block_on(crate::db::get_settings_override(&db, 7))
        .unwrap()
        .unwrap();
    assert_eq!(ov.prefix.as_deref(), Some("?"));
    let err = tokio_test_block_on(crate::db::patch_settings(
        &db,
        7,
        Some(Some("   ".into())),
        None,
        None,
    ))
    .unwrap_err();
    assert!(
        err.to_string().contains("prefix cannot be empty"),
        "whitespace-only prefix must be rejected: {err:#}"
    );
    cleanup_db(&tmp);
}

#[test]
fn terminal_defaults_come_from_durable_meta_not_process_config() {
    let tmp = tmp_db_path("terminal_defaults_meta");
    // Bot migrate-open writes meta terminal defaults from its config.
    {
        let db = Db::open_with(DbOpenOptions {
            path: tmp.clone(),
            url: String::new(),
            token: String::new(),
            pool_size: 2,
            migrate: true,
            default_prefix: "bot!".into(),
            retention_days: 21,
        })
        .unwrap();
        tokio_test_block_on(db.with_conn(|conn| {
            // Empty global override → effective falls through to terminal defaults.
            conn.execute(
                "UPDATE guild_settings SET settings = '{}' WHERE guild_id = 0",
                (),
            )?;
            Ok(())
        }))
        .unwrap();
        assert_eq!(db.read_terminal_defaults().unwrap().prefix, "bot!");
        assert_eq!(db.read_terminal_defaults().unwrap().retention_days, 21);
    }
    // API opens with *different* process config — must still use bot's meta.
    let api = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "api!".into(),
        retention_days: 3,
    })
    .unwrap();
    assert_eq!(
        api.read_terminal_defaults().unwrap().prefix,
        "bot!",
        "API must not override durable terminal defaults from its own config"
    );
    assert_eq!(api.read_terminal_defaults().unwrap().retention_days, 21);
    let effective = tokio_test_block_on(crate::db::get_settings(&api, 99)).unwrap();
    assert_eq!(effective.prefix, "bot!");
    assert_eq!(effective.retention_days, 21);
    let parts = tokio_test_block_on(crate::db::load_config_parts(&api, 99)).unwrap();
    assert_eq!(parts.settings.prefix, "bot!");
    assert_eq!(parts.settings.retention_days, 21);
    cleanup_db(&tmp);
}

#[test]
fn runtime_read_ignores_corrupt_settings_and_bot_heals_atomically() {
    let tmp = tmp_db_path("runtime_corrupt_clear");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (44, ?1)",
            params!["{not-json"],
        )?;
        Ok(())
    }))
    .unwrap();
    // Runtime reads inherit safely without performing a multi-commit repair.
    let effective = tokio_test_block_on(crate::db::get_settings(&db, 44)).unwrap();
    assert_eq!(
        effective.prefix,
        db.read_terminal_defaults().unwrap().prefix
    );
    assert!(
        tokio_test_block_on(crate::db::get_settings_override(&db, 44))
            .unwrap()
            .is_none(),
        "corrupt row must not become an effective override"
    );
    let raw: String = db
        .with_conn_sync(|conn| {
            conn.query_row(
                "SELECT settings FROM guild_settings WHERE guild_id = 44",
                (),
                |row| Ok(row.get(0)?),
            )
        })
        .unwrap();
    assert_eq!(raw, "{not-json");
    let v_before_heal = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    drop(db);
    let db2 = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    let v_after_heal = tokio_test_block_on(crate::db::get_config_version(&db2)).unwrap();
    assert!(
        v_after_heal > v_before_heal,
        "bot heal must publish its repair"
    );
    cleanup_db(&tmp);
}

#[test]
fn api_open_refuses_missing_terminal_defaults_meta() {
    let tmp = tmp_db_path("api_no_terminal_meta");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM meta WHERE key = 'settings_terminal_defaults'",
            (),
        )?;
        Ok(())
    }))
    .unwrap();
    drop(db);
    let err = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .err()
    .expect("API must refuse when terminal defaults meta is missing");
    assert!(
        err.to_string().contains("settings_terminal_defaults"),
        "unexpected: {err:#}"
    );
    cleanup_db(&tmp);
}

#[test]
fn heal_skips_when_guild_settings_unchanged() {
    let tmp = tmp_db_path("heal_checksum_skip");
    let db = Db::open(&tmp, 2).unwrap();
    let v_after_first = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    let checksum_after_first = db
        .with_conn_sync(|conn| {
            Ok(meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?
                .and_then(|s| s.parse::<u64>().ok()))
        })
        .unwrap();
    drop(db);
    let db2 = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    let v_after_second = tokio_test_block_on(crate::db::get_config_version(&db2)).unwrap();
    assert_eq!(
        v_after_first, v_after_second,
        "second open must skip heal when guild_settings checksum unchanged"
    );
    let checksum_after_second = db2
        .with_conn_sync(|conn| {
            Ok(meta_string_conn(conn, META_SETTINGS_HEAL_CHECKSUM)?
                .and_then(|s| s.parse::<u64>().ok()))
        })
        .unwrap();
    assert_eq!(
        checksum_after_first, checksum_after_second,
        "heal checksum must be stable across process opens (not DefaultHasher)"
    );
    // Recompute must match stored meta (FNV is deterministic).
    let recomputed = db2
        .with_conn_sync(|conn| guild_settings_checksum_conn(conn))
        .unwrap();
    assert_eq!(checksum_after_second, Some(recomputed));
    cleanup_db(&tmp);
}

#[test]
fn delete_noop_does_not_bump_config_or_slash_versions() {
    let tmp = tmp_db_path("delete_noop");
    let db = Db::open(&tmp, 2).unwrap();
    let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    let s0 = tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap();
    // Delete a command / rule that was never inserted.
    tokio_test_block_on(crate::db::delete_command(&db, 99, "nope")).unwrap();
    tokio_test_block_on(crate::db::delete_rule(&db, 99, "mention_spam")).unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_config_version(&db)).unwrap(),
        v0,
        "no-op delete must not bump config_version"
    );
    assert_eq!(
        tokio_test_block_on(crate::db::get_slash_registry_version(&db)).unwrap(),
        s0,
        "no-op command delete must not bump slash_registry_version"
    );
    cleanup_db(&tmp);
}

#[test]
fn seeded_commands_publish_a_primary_registry_version() {
    let tmp = tmp_db_path("seed_registry_version");
    let db = Db::open(&tmp, 2).unwrap();
    let versions = get_primary_versions_sync(&db).unwrap();
    assert!(
        versions.config > 0,
        "atomic seed must publish config visibility"
    );
    assert!(
        versions.slash > 0,
        "atomic command seed must publish slash visibility"
    );
    cleanup_db(&tmp);
}

#[test]
fn nickname_outbox_generation_cannot_ack_a_newer_write() {
    let tmp = tmp_db_path("nickname_outbox_generation");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(patch_settings(
        &db,
        42,
        None,
        Some(Some("first".into())),
        None,
    ))
    .unwrap();
    let first = tokio_test_block_on(pending_nickname_reconciles(&db, 0, 25)).unwrap();
    assert_eq!(first[0].nickname, "first");
    assert_eq!((first[0].guild_id, first[0].generation), (42, 1));

    tokio_test_block_on(patch_settings(
        &db,
        42,
        None,
        Some(Some("second".into())),
        None,
    ))
    .unwrap();
    tokio_test_block_on(acknowledge_nickname_reconcile(&db, 42, 1)).unwrap();
    let second = tokio_test_block_on(pending_nickname_reconciles(&db, 0, 25)).unwrap();
    assert_eq!(second[0].nickname, "second");
    assert_eq!((second[0].guild_id, second[0].generation), (42, 2));
    tokio_test_block_on(acknowledge_nickname_reconcile(&db, 42, 2)).unwrap();
    assert!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 0, 25))
            .unwrap()
            .is_empty()
    );
    cleanup_db(&tmp);
}

#[test]
fn nickname_reconcile_exists_before_settings_commit() {
    let tmp = tmp_db_path("nickname_outbox_precommit");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(request_nickname_reconcile(&db, 43)).unwrap();
    assert_eq!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 0, 25)).unwrap()[0].nickname,
        String::new(),
        "a cancelled Discord-first request must retain work for the old authoritative value"
    );
    cleanup_db(&tmp);
}

#[test]
fn nickname_outbox_migration_queues_existing_guild_nicknames() {
    let tmp = tmp_db_path("nickname_outbox_migration");
    let legacy = Connection::open(&tmp).unwrap();
    for (index, sql) in MIGRATIONS[..4].iter().enumerate() {
        legacy.execute_batch(sql).unwrap();
        legacy
            .pragma_update(None, "user_version", (index + 1) as i64)
            .unwrap();
    }
    legacy
        .execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
            params![42i64, r#"{"bot_name":"Legacy nickname"}"#],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
            params![43i64, r#"{"bot_name":""}"#],
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO guild_settings (guild_id, settings) VALUES (?1, ?2)",
            params![0i64, r#"{"bot_name":"Global nickname"}"#],
        )
        .unwrap();
    drop(legacy);

    let db = Db::open(&tmp, 2).unwrap();
    let pending = tokio_test_block_on(pending_nickname_reconciles(&db, 0, 25)).unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|job| (job.guild_id, job.nickname.as_str()))
            .collect::<Vec<_>>(),
        vec![(42, "Legacy nickname")],
        "migration must bridge pre-outbox guild-local nickname configuration"
    );
    cleanup_db(&tmp);
}

#[test]
fn nickname_reconcile_persists_retry_terminal_and_dormant_lifecycle() {
    let tmp = tmp_db_path("nickname_outbox_lifecycle");
    let db = Db::open(&tmp, 2).unwrap();
    tokio_test_block_on(request_nickname_reconcile(&db, 44)).unwrap();
    let mut job = tokio_test_block_on(pending_nickname_reconciles(&db, 100, 25))
        .unwrap()
        .remove(0);
    assert!(
        !tokio_test_block_on(schedule_nickname_reconcile_retry(
            &db,
            &job,
            100,
            "transport",
            "temporary"
        ))
        .unwrap()
    );
    assert!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 104, 25))
            .unwrap()
            .is_empty(),
        "transient work must wait for its persisted schedule"
    );
    job = tokio_test_block_on(pending_nickname_reconciles(&db, 10_000, 25))
        .unwrap()
        .remove(0);
    tokio_test_block_on(finish_nickname_reconcile(
        &db,
        &job,
        "terminal",
        "discord_50013",
        "missing permissions",
        10_000,
    ))
    .unwrap();
    assert!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
            .unwrap()
            .is_empty(),
        "terminal work must not make another API attempt"
    );
    tokio_test_block_on(activate_nickname_reconcile_for_guild_create(&db, 44, false)).unwrap();
    assert_eq!(
        tokio_test_block_on(nickname_reconcile_status(&db, 44))
            .unwrap()
            .unwrap()
            .state,
        "terminal",
        "a gateway reconnect must preserve an operator-actionable terminal failure"
    );

    tokio_test_block_on(request_nickname_reconcile(&db, 45)).unwrap();
    tokio_test_block_on(dormancy_nickname_reconcile(&db, 45, "departed")).unwrap();
    assert!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
            .unwrap()
            .is_empty(),
        "departed guild work must remain dormant"
    );
    tokio_test_block_on(activate_nickname_reconcile_for_guild_create(&db, 45, false)).unwrap();
    assert_eq!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
            .unwrap()
            .len(),
        1,
        "guild_create must reactivate dormant work without resetting terminal rows"
    );

    tokio_test_block_on(activate_nickname_reconcile_for_guild_create(&db, 46, true)).unwrap();
    assert_eq!(
        tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
            .unwrap()
            .iter()
            .filter(|job| job.guild_id == 46)
            .count(),
        1,
        "a genuine first guild join must create initial reconciliation work"
    );

    tokio_test_block_on(request_nickname_reconcile(&db, 47)).unwrap();
    let in_flight = tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
        .unwrap()
        .into_iter()
        .find(|job| job.guild_id == 47)
        .unwrap();
    tokio_test_block_on(dormancy_nickname_reconcile(
        &db,
        47,
        "departed during request",
    ))
    .unwrap();
    tokio_test_block_on(finish_nickname_reconcile(
        &db,
        &in_flight,
        "terminal",
        "late_error",
        "late Discord response",
        20_001,
    ))
    .unwrap();
    tokio_test_block_on(acknowledge_nickname_reconcile(
        &db,
        in_flight.guild_id,
        in_flight.generation,
    ))
    .unwrap();
    assert_eq!(
        tokio_test_block_on(nickname_reconcile_status(&db, 47))
            .unwrap()
            .unwrap()
            .state,
        "dormant",
        "late completion of an invalidated generation must not erase departure state"
    );

    tokio_test_block_on(request_nickname_reconcile(&db, 48)).unwrap();
    let terminal = tokio_test_block_on(pending_nickname_reconciles(&db, 20_000, 25))
        .unwrap()
        .into_iter()
        .find(|job| job.guild_id == 48)
        .unwrap();
    tokio_test_block_on(finish_nickname_reconcile(
        &db,
        &terminal,
        "terminal",
        "discord_50013",
        "missing permissions",
        20_000,
    ))
    .unwrap();
    assert!(
        tokio_test_block_on(patch_settings_with_nickname_reconcile(
            &db,
            48,
            Some(Some(" ".into())),
            None,
            None,
            true,
        ))
        .is_err()
    );
    assert_eq!(
        tokio_test_block_on(nickname_reconcile_status(&db, 48))
            .unwrap()
            .unwrap()
            .state,
        "terminal",
        "a rejected settings request must not reset terminal nickname work"
    );
    cleanup_db(&tmp);
}

#[test]
fn bot_bootstrap_upgrades_only_unchanged_legacy_setname_seed() {
    let tmp = tmp_db_path("legacy_setname_seed");
    let db = Db::open(&tmp, 2).unwrap();
    let mut legacy = tokio_test_block_on(get_command_row(&db, GLOBAL_SCOPE, "setname"))
        .unwrap()
        .unwrap();
    legacy.description =
        "Set the bot's nickname in this server (omit / clear / - to reset).".into();
    legacy.response = "Bot nickname set to {name}".into();
    tokio_test_block_on(upsert_command(&db, GLOBAL_SCOPE, "setname", &legacy)).unwrap();
    drop(db);

    let reopened = Db::open(&tmp, 2).unwrap();
    let upgraded = tokio_test_block_on(get_command_row(&reopened, GLOBAL_SCOPE, "setname"))
        .unwrap()
        .unwrap();
    assert_eq!(upgraded.response, "Bot nickname queued: {name}");
    assert!(upgraded.description.starts_with("Queue "));
    cleanup_db(&tmp);
}

#[test]
fn nickname_validation_rejects_permanent_discord_failures_before_enqueue() {
    let tmp = tmp_db_path("nickname_validation");
    let db = Db::open(&tmp, 2).unwrap();
    assert!(
        tokio_test_block_on(patch_settings(
            &db,
            46,
            None,
            Some(Some("x".repeat(33))),
            None,
        ))
        .is_err()
    );
    assert!(
        tokio_test_block_on(pending_nickname_reconciles(&db, i64::MAX, 25))
            .unwrap()
            .is_empty()
    );
    cleanup_db(&tmp);
}

#[test]
fn badword_view_stringifies_guild_snowflake() {
    let snow = 1329331947876479006i64;
    let view = Badword {
        id: 1,
        guild_id: snow,
        pattern: "scam".into(),
        note: None,
    }
    .with_status();
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["guild_id"], "1329331947876479006");
    assert!(json["guild_id"].is_string());
    let global = Badword {
        id: 2,
        guild_id: 0,
        pattern: "x".into(),
        note: None,
    }
    .with_status();
    let g = serde_json::to_value(&global).unwrap();
    assert_eq!(g["guild_id"], "0");
}

#[test]
fn bot_terminal_defaults_update_visible_without_api_restart() {
    let tmp = tmp_db_path("terminal_defaults_live");
    let db = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "a!".into(),
        retention_days: 7,
    })
    .unwrap();
    tokio_test_block_on(db.with_conn(|conn| {
        conn.execute(
            "UPDATE guild_settings SET settings = '{}' WHERE guild_id = 0",
            (),
        )?;
        Ok(())
    }))
    .unwrap();
    let api = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "wrong".into(),
        retention_days: 99,
    })
    .unwrap();
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&api, 1))
            .unwrap()
            .prefix,
        "a!"
    );
    drop(db);
    // Bot migrate-open rewrites terminal defaults; keep handle only for side effect.
    let _bot = Db::open_with(DbOpenOptions {
        path: tmp.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: true,
        default_prefix: "b!".into(),
        retention_days: 7,
    })
    .unwrap();
    // API process still open — must see bot's new meta without restart.
    assert_eq!(
        tokio_test_block_on(crate::db::get_settings(&api, 1))
            .unwrap()
            .prefix,
        "b!"
    );
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_rejects_empty_prefix() {
    let tmp = tmp_db_path("empty_prefix");
    let db = Db::open(&tmp, 2).unwrap();
    let err = tokio_test_block_on(crate::db::patch_settings(
        &db,
        GLOBAL_SCOPE,
        Some(Some(String::new())),
        None,
        None,
    ))
    .err()
    .expect("empty prefix must fail");
    assert!(
        err.to_string().contains("prefix cannot be empty"),
        "unexpected: {err:#}"
    );
    cleanup_db(&tmp);
}

#[test]
fn patch_settings_noop_does_not_bump_config_version() {
    let tmp = tmp_db_path("patch_noop");
    let db = Db::open(&tmp, 2).unwrap();
    let v0 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    tokio_test_block_on(crate::db::patch_settings(&db, 1, None, None, None)).unwrap();
    let v1 = tokio_test_block_on(crate::db::get_config_version(&db)).unwrap();
    assert_eq!(v0, v1, "empty patch must not bump config_version");
    cleanup_db(&tmp);
}

mod schema;

mod action_history;
mod action_queue;

mod enrollments;

mod action_enrollments;
