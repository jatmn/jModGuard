use super::*;
struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("configuration_enrollment");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn scope(&self, guild: i64) -> ConfigurationScope {
        ConfigurationScope::Guild(enroll_guild(&self.db, guild, 100).await.unwrap().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}

#[tokio::test]
async fn stale_settings_and_empty_patches_cannot_write_or_read_after_departure_or_rejoin() {
    for rejoin in [false, true] {
        let f = Fixture::new();
        placed_before_erasure(&f.db, 10).await;
        let scope = f.scope(10).await;
        patch_enrolled_settings(
            &f.db,
            scope.clone(),
            Some(Some("initial".into())),
            Some(Some("initial".into())),
            false,
        )
        .await
        .unwrap();
        retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
        if rejoin {
            enroll_guild(&f.db, 10, 101).await.unwrap();
        }
        let before = get_primary_versions(&f.db).await.unwrap();
        for (prefix, nickname, retry) in [
            (
                Some(Some("stale".into())),
                Some(Some("stale".into())),
                false,
            ),
            (None, None, false),
            (None, None, true),
        ] {
            let error = patch_enrolled_settings(&f.db, scope.clone(), prefix, nickname, retry)
                .await
                .unwrap_err();
            assert!(error.is::<EnrollmentChanged>(), "{error:#}");
        }
        assert_eq!(get_settings(&f.db, 10).await.unwrap().prefix, "initial");
        assert!(
            pending_nickname_reconciles(&f.db, i64::MAX, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let after = get_primary_versions(&f.db).await.unwrap();
        assert_eq!(before.config, after.config);
        assert_eq!(before.config_revision, after.config_revision);
    }
}

#[tokio::test]
async fn valid_current_scope_preserves_atomic_nickname_and_noop_version_semantics() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    patch_enrolled_settings(
        &f.db,
        scope.clone(),
        None,
        Some(Some("current".into())),
        false,
    )
    .await
    .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    let jobs = pending_nickname_reconciles(&f.db, 0, 1).await.unwrap();
    assert_eq!(jobs[0].nickname, "current");
    assert_eq!(
        patch_enrolled_settings(&f.db, scope, None, None, false)
            .await
            .unwrap()
            .bot_name,
        "current"
    );
    let after = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(
        jobs,
        pending_nickname_reconciles(&f.db, 0, 1).await.unwrap()
    );
    patch_enrolled_settings(
        &f.db,
        ConfigurationScope::Global,
        Some(Some("global".into())),
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(get_settings(&f.db, 0).await.unwrap().prefix, "global");
}

#[tokio::test]
async fn copy_checks_both_authorized_memberships_before_any_replacement() {
    for changed_guild in [10, 20] {
        let f = Fixture::new();
        let source = f.scope(10).await;
        let destination = f.scope(20).await;
        patch_enrolled_settings(
            &f.db,
            source.clone(),
            Some(Some("source".into())),
            Some(Some("source".into())),
            false,
        )
        .await
        .unwrap();
        patch_enrolled_settings(
            &f.db,
            destination.clone(),
            Some(Some("destination".into())),
            None,
            false,
        )
        .await
        .unwrap();
        enroll_guild(&f.db, changed_guild, 101).await.unwrap();
        let before = get_primary_versions(&f.db).await.unwrap();
        let error = copy_enrolled_guild_config(&f.db, source, destination, vec!["all".into()])
            .await
            .unwrap_err();
        assert!(error.is::<EnrollmentChanged>());
        assert_eq!(get_settings(&f.db, 20).await.unwrap().prefix, "destination");
        assert!(
            pending_nickname_reconciles(&f.db, 0, 10)
                .await
                .unwrap()
                .iter()
                .all(|job| job.guild_id != 20)
        );
        assert_eq!(
            before.config_revision,
            get_primary_versions(&f.db).await.unwrap().config_revision
        );
    }
}

#[tokio::test]
async fn copying_into_global_still_checks_source_and_current_copy_succeeds() {
    let f = Fixture::new();
    let source = f.scope(10).await;
    let destination = f.scope(20).await;
    patch_enrolled_settings(
        &f.db,
        source.clone(),
        Some(Some("source".into())),
        None,
        false,
    )
    .await
    .unwrap();
    copy_enrolled_guild_config(&f.db, source.clone(), destination, vec!["settings".into()])
        .await
        .unwrap();
    assert_eq!(get_settings(&f.db, 20).await.unwrap().prefix, "source");
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(
        copy_enrolled_guild_config(
            &f.db,
            source,
            ConfigurationScope::Global,
            vec!["settings".into()]
        )
        .await
        .unwrap_err()
        .is::<EnrollmentChanged>()
    );
    assert_ne!(get_settings(&f.db, 0).await.unwrap().prefix, "source");
}

#[tokio::test]
async fn modified_scope_identity_cannot_target_another_guild_or_global() {
    let f = Fixture::new();
    let ConfigurationScope::Guild(enrollment) = f.scope(10).await else {
        panic!()
    };
    f.scope(20).await;
    for field in 0..4 {
        let mut forged = enrollment.clone();
        match field {
            0 => forged.guild_id = 0,
            1 => forged.guild_id = 20,
            2 => forged.id = "forged".into(),
            _ => forged.joined_at_ns += 1,
        };
        assert!(
            patch_enrolled_settings(
                &f.db,
                ConfigurationScope::Guild(forged),
                Some(Some("forged".into())),
                None,
                false
            )
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
        );
    }
    for guild in [0, 10, 20] {
        assert_ne!(get_settings(&f.db, guild).await.unwrap().prefix, "forged");
    }
}

#[tokio::test]
async fn unavailable_membership_check_returns_a_safe_error_and_no_settings_change() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "ALTER TABLE guild_enrollments RENAME TO test_hidden_table",
            64 * 1024 * 1024,
        )
    })
    .await
    .unwrap();
    let error = patch_enrolled_settings(&f.db, scope, Some(Some("rejected".into())), None, false)
        .await
        .unwrap_err();
    assert!(error.is::<EnrollmentUnverified>());
    assert!(!error.to_string().contains("table"));
    assert_ne!(get_settings(&f.db, 10).await.unwrap().prefix, "rejected");
}

fn edit_scope(request: &ConfigurationScope, global: bool) -> ConfigurationWriteScope {
    ConfigurationWriteScope::new(
        request.clone(),
        if global {
            ConfigurationScope::Global
        } else {
            request.clone()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn stale_configuration_writes_preserve_rows_and_both_version_markers() {
    for global in [false, true] {
        for rejoin in [false, true] {
            let f = Fixture::new();
            placed_before_erasure(&f.db, 10).await;
            let request = f.scope(10).await;
            let target = if global { 0 } else { 10 };
            let rule = crate::defaults::default_rules().remove(0);
            let command = crate::defaults::default_commands().remove(0);
            upsert_rule(&f.db, target, "fixture", &rule).await.unwrap();
            upsert_command(&f.db, target, "fixture", &command)
                .await
                .unwrap();
            upsert_template(&f.db, target, "message_edit", "preserved")
                .await
                .unwrap();
            let word = add_badword(&f.db, target, "preserved", None).await.unwrap();
            retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
            if rejoin {
                enroll_guild(&f.db, 10, 101).await.unwrap();
            }
            let before = get_primary_versions(&f.db).await.unwrap();
            let scope = edit_scope(&request, global);
            let results = [
                upsert_enrolled_rule(&f.db, scope.clone(), "new", &rule).await,
                delete_enrolled_rule(&f.db, scope.clone(), "fixture").await,
                upsert_enrolled_command(&f.db, scope.clone(), "new", &command).await,
                delete_enrolled_command(&f.db, scope.clone(), "fixture").await,
                upsert_enrolled_template(&f.db, scope.clone(), "message_edit", "changed").await,
                delete_enrolled_template(&f.db, scope.clone(), "message_edit").await,
                add_enrolled_badword(&f.db, scope.clone(), "changed", None)
                    .await
                    .map(|_| ()),
                delete_enrolled_badword(&f.db, scope, word).await,
            ];
            for result in results {
                assert!(result.unwrap_err().is::<EnrollmentChanged>());
            }
            assert!(get_rule(&f.db, target, "new").await.unwrap().is_none());
            assert!(get_rule(&f.db, target, "fixture").await.unwrap().is_some());
            assert!(
                get_command_row(&f.db, target, "new")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                get_command_row(&f.db, target, "fixture")
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                get_template_row(&f.db, target, "message_edit")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("preserved")
            );
            assert!(get_badword_primary(&f.db, word).await.unwrap().is_some());
            assert!(
                list_badwords(&f.db, target)
                    .await
                    .unwrap()
                    .iter()
                    .all(|word| word.pattern != "changed")
            );
            let after = get_primary_versions(&f.db).await.unwrap();
            assert_eq!(before.config_revision, after.config_revision);
            assert_eq!(before.slash_revision, after.slash_revision);
        }
    }
}

#[tokio::test]
async fn current_configuration_edits_preserve_validation_noops_and_exact_badword_scope() {
    let f = Fixture::new();
    let request = f.scope(10).await;
    let other = f.scope(20).await;
    let scope = edit_scope(&request, false);
    let rule = crate::defaults::default_rules().remove(0);
    let command = crate::defaults::default_commands().remove(0);
    upsert_enrolled_rule(&f.db, scope.clone(), "fixture", &rule)
        .await
        .unwrap();
    upsert_enrolled_template(&f.db, scope.clone(), "message_edit", "current")
        .await
        .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    upsert_enrolled_command(&f.db, scope.clone(), "fixture", &command)
        .await
        .unwrap();
    let after = get_primary_versions(&f.db).await.unwrap();
    assert!(after.slash > before.slash);
    assert!(after.config > before.config);
    let word = add_enrolled_badword(&f.db, scope.clone(), "current", None)
        .await
        .unwrap();
    let error = delete_enrolled_badword(&f.db, edit_scope(&other, false), word)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "badword not found");
    assert_eq!(
        delete_enrolled_badword(&f.db, scope.clone(), i64::MAX)
            .await
            .unwrap_err()
            .to_string(),
        "badword not found"
    );
    assert!(
        add_enrolled_badword(&f.db, scope.clone(), "[", None)
            .await
            .is_err()
    );
    delete_enrolled_rule(&f.db, scope.clone(), "fixture")
        .await
        .unwrap();
    delete_enrolled_template(&f.db, scope.clone(), "message_edit")
        .await
        .unwrap();
    delete_enrolled_command(&f.db, scope.clone(), "fixture")
        .await
        .unwrap();
    delete_enrolled_badword(&f.db, scope.clone(), word)
        .await
        .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    delete_enrolled_rule(&f.db, scope.clone(), "fixture")
        .await
        .unwrap();
    delete_enrolled_template(&f.db, scope.clone(), "message_edit")
        .await
        .unwrap();
    delete_enrolled_command(&f.db, scope, "fixture")
        .await
        .unwrap();
    let after = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(before.config_revision, after.config_revision);
    assert_eq!(before.slash_revision, after.slash_revision);
}

#[tokio::test]
async fn write_scopes_reject_foreign_targets_and_guard_failures_roll_back() {
    let f = Fixture::new();
    let request = f.scope(10).await;
    let other = f.scope(20).await;
    assert!(ConfigurationWriteScope::new(request.clone(), other).is_err());
    assert!(ConfigurationWriteScope::new(ConfigurationScope::Global, request.clone()).is_err());
    f.db.with_conn(|conn|conn.execute_batch_bounded("CREATE TRIGGER refuse_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'fixture'); END", 64 * 1024 * 1024)).await.unwrap();
    let scope = edit_scope(&request, false);
    assert!(
        upsert_enrolled_template(&f.db, scope, "message_edit", "rejected")
            .await
            .is_err()
    );
    assert!(
        get_template_row(&f.db, 10, "message_edit")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn queued_settings_request_rechecks_membership_after_waiting_for_writer_lock() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let lock = f.db.settings_write_lock.clone().lock_owned().await;
    let db = f.db.clone();
    let pending = tokio::spawn(async move {
        patch_enrolled_settings(
            &db,
            scope,
            Some(Some("stale".into())),
            Some(Some("stale".into())),
            false,
        )
        .await
    });
    tokio::task::yield_now().await;
    enroll_guild(&f.db, 10, 101).await.unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    drop(lock);
    assert!(
        pending
            .await
            .unwrap()
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert_ne!(get_settings(&f.db, 10).await.unwrap().prefix, "stale");
    assert!(
        pending_nickname_reconciles(&f.db, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        get_primary_versions(&f.db).await.unwrap().config_revision,
        before.config_revision
    );
}

#[tokio::test]
async fn invalid_copy_replacement_rolls_back_settings_nickname_and_versions() {
    let f = Fixture::new();
    let source = f.scope(10).await;
    let destination = f.scope(20).await;
    patch_enrolled_settings(
        &f.db,
        source.clone(),
        Some(Some("source".into())),
        Some(Some("source".into())),
        false,
    )
    .await
    .unwrap();
    patch_enrolled_settings(
        &f.db,
        destination.clone(),
        Some(Some("destination".into())),
        Some(Some("destination".into())),
        false,
    )
    .await
    .unwrap();
    f.db.with_conn(|conn| {
        conn.execute_bounded(
            "INSERT INTO badwords(guild_id,pattern) VALUES(10,'[')",
            (),
            64 * 1024 * 1024,
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let word = add_badword(&f.db, 20, "preserved", None).await.unwrap();
    let jobs = pending_nickname_reconciles(&f.db, 0, 10).await.unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    assert!(
        copy_enrolled_guild_config(&f.db, source, destination, vec!["all".into()])
            .await
            .is_err()
    );
    let settings = get_settings(&f.db, 20).await.unwrap();
    assert_eq!(settings.prefix, "destination");
    assert_eq!(settings.bot_name, "destination");
    assert!(get_badword_primary(&f.db, word).await.unwrap().is_some());
    assert_eq!(
        pending_nickname_reconciles(&f.db, 0, 10).await.unwrap(),
        jobs
    );
    let after = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(after.config_revision, before.config_revision);
    assert_eq!(after.slash_revision, before.slash_revision);
}

#[tokio::test]
async fn configuration_copy_cannot_write_a_fenced_destination() {
    let f = Fixture::new();
    let source = f.scope(10).await;
    let destination = f.scope(20).await;
    upsert_template(&f.db, 10, "notice", "source")
        .await
        .unwrap();
    upsert_template(&f.db, 20, "notice", "destination")
        .await
        .unwrap();
    let store = crate::db::guild_stores::StoreId::new();
    crate::db::guild_stores::register_store(
        &f.db,
        store.clone(),
        crate::db::guild_stores::StorePolicy::Dedicated { guild_id: 20 },
    )
    .await
    .unwrap();
    {
        use crate::db::SqlExecutor as _;
        let destination = store;
        f.db.with_conn(move |conn| {
            conn.execute_bounded(
                "INSERT INTO guild_write_fences(
                    guild_id,generation,phase,destination_store_id,destination_revision,created_at
                 ) VALUES(?1,?2,'freeze',?3,?2,unixepoch())",
                libsql::params![20, format!("{:032x}", 9u128), destination.as_str()],
                64 * 1024 * 1024,
            )?;
            Ok(())
        })
    }
    .await
    .unwrap();

    assert!(
        copy_enrolled_guild_config(&f.db, source, destination, vec!["templates".into()])
            .await
            .is_err()
    );
    assert_eq!(
        get_template_row(&f.db, 20, "notice").await.unwrap(),
        Some("destination".into())
    );
}

#[tokio::test]
async fn known_bad_images_are_durable_scoped_validated_and_noop_safe() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let other = f.scope(20).await;
    let write = ConfigurationWriteScope::new(scope.clone(), scope.clone()).unwrap();
    let hash = "ab".repeat(32);
    let before = get_config_version(&f.db).await.unwrap();
    for invalid in [
        "".to_owned(),
        "unknown".into(),
        "g".repeat(64),
        "a".repeat(63),
    ] {
        assert!(
            add_enrolled_known_bad_image(&f.db, write.clone(), &invalid, None)
                .await
                .is_err()
        );
    }
    assert_eq!(get_config_version(&f.db).await.unwrap(), before);
    assert!(
        add_enrolled_known_bad_image(
            &f.db,
            write.clone(),
            &hash.to_uppercase(),
            Some("confirmed spam".into())
        )
        .await
        .unwrap()
    );
    let added_version = get_config_version(&f.db).await.unwrap();
    assert!(added_version > before);
    assert!(
        !add_enrolled_known_bad_image(&f.db, write.clone(), &hash, Some("duplicate".into()))
            .await
            .unwrap()
    );
    assert_eq!(get_config_version(&f.db).await.unwrap(), added_version);
    let snapshot = load_enrolled_web_config_snapshot(&f.db, scope.clone())
        .await
        .unwrap();
    let rows = snapshot.known_bad_images;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].hash, hash);
    assert_eq!(rows[0].note.as_deref(), Some("confirmed spam"));
    assert_eq!(rows[0].actor, "unknown");
    assert_eq!(rows[0].added_via, "webui");
    assert!(chrono::DateTime::parse_from_rfc3339(&rows[0].added_at).is_ok());
    assert!(
        load_enrolled_web_config_snapshot(&f.db, other.clone())
            .await
            .unwrap()
            .known_bad_images
            .is_empty()
    );
    let reloaded = Db::open(&f.path, 2).unwrap();
    assert_eq!(
        load_config_parts(&reloaded, 10)
            .await
            .unwrap()
            .known_bad_images,
        rows
    );
    let foreign = ConfigurationWriteScope::new(other.clone(), other).unwrap();
    assert!(
        !remove_enrolled_known_bad_image(&f.db, foreign, &hash)
            .await
            .unwrap()
    );
    assert_eq!(get_config_version(&f.db).await.unwrap(), added_version);
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    enroll_guild(&f.db, 10, 101).await.unwrap();
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        add_enrolled_known_bad_image(&f.db, write, &hash, None)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    let fresh = ConfigurationScope::Guild(guild_enrollment(&f.db, 10).await.unwrap().unwrap());
    let write = ConfigurationWriteScope::new(fresh.clone(), fresh).unwrap();
    assert!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .is_empty(),
        "departure erases the guild list"
    );
    assert!(
        add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None)
            .await
            .unwrap()
    );
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash)
            .await
            .unwrap()
    );
    let removed_version = get_config_version(&f.db).await.unwrap();
    assert!(removed_version > added_version);
    assert!(
        !remove_enrolled_known_bad_image(&f.db, write, &hash)
            .await
            .unwrap()
    );
    assert_eq!(get_config_version(&f.db).await.unwrap(), removed_version);
    let global =
        ConfigurationWriteScope::new(ConfigurationScope::Global, ConfigurationScope::Global)
            .unwrap();
    assert!(
        add_enrolled_known_bad_image(&f.db, global, &hash, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn known_bad_image_admission_bounds_and_version_failure_are_atomic() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let write = ConfigurationWriteScope::new(scope.clone(), scope).unwrap();
    let hash = "cd".repeat(32);
    assert!(
        add_enrolled_known_bad_image(&f.db, write.clone(), &hash, Some("x".repeat(257)))
            .await
            .is_err()
    );
    assert!(safe_image_note(Some("9".repeat(15))).is_err());
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded("CREATE TRIGGER refuse_hash_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'fixture'); END", 4096)
    }).await.unwrap();
    assert!(
        add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None)
            .await
            .is_err()
    );
    assert!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .is_empty()
    );
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded("DROP TRIGGER refuse_hash_version", 4096)?;
        let tx = conn.transaction_immediate()?;
        for n in 0..MAX_KNOWN_BAD_IMAGES {
            tx.execute_rows(
                "INSERT INTO known_bad_images VALUES (?1, ?2, 'fixture', 'webui', 'unknown', NULL)",
                params![10, format!("{n:064x}")],
                1,
            )?;
        }
        tx.commit()
    })
    .await
    .unwrap();
    assert!(
        add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None)
            .await
            .is_err()
    );
    let present = format!("{:064x}", 0);
    assert!(
        !add_enrolled_known_bad_image(&f.db, write.clone(), &present, None)
            .await
            .unwrap()
    );
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &present)
            .await
            .unwrap()
    );
    assert!(
        add_enrolled_known_bad_image(&f.db, write, &hash, None)
            .await
            .unwrap()
    );
    assert_eq!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .len(),
        MAX_KNOWN_BAD_IMAGES
    );
}

#[tokio::test]
async fn known_bad_image_remove_version_failure_rolls_back_and_retries() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let write = ConfigurationWriteScope::new(scope.clone(), scope).unwrap();
    let hash = "ab".repeat(32);
    add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None)
        .await
        .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "CREATE TRIGGER refuse_hash_remove_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'fixture'); END",
            4096,
        )
    })
    .await
    .unwrap();
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash)
            .await
            .is_err()
    );
    assert_eq!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .len(),
        1
    );
    let unchanged = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(unchanged.config, before.config);
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded("DROP TRIGGER refuse_hash_remove_version", 4096)
    })
    .await
    .unwrap();
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash)
            .await
            .unwrap()
    );
    assert!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .is_empty()
    );
}

#[tokio::test]
async fn known_bad_image_remove_cannot_write_a_fenced_guild() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let write = ConfigurationWriteScope::new(scope.clone(), scope).unwrap();
    let hash = "ab".repeat(32);
    add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None)
        .await
        .unwrap();
    let store = crate::db::guild_stores::StoreId::new();
    crate::db::guild_stores::register_store(
        &f.db,
        store.clone(),
        crate::db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
    )
    .await
    .unwrap();
    {
        use crate::db::SqlExecutor as _;
        let destination = store;
        f.db.with_conn(move |conn| {
            conn.execute_bounded(
                "INSERT INTO guild_write_fences(
                    guild_id,generation,phase,destination_store_id,destination_revision,created_at
                 ) VALUES(?1,?2,'freeze',?3,?2,unixepoch())",
                libsql::params![10, format!("{:032x}", 9u128), destination.as_str()],
                64 * 1024 * 1024,
            )?;
            Ok(())
        })
    }
    .await
    .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    assert!(
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash)
            .await
            .is_err()
    );
    assert_eq!(
        load_config_parts(&f.db, 10)
            .await
            .unwrap()
            .known_bad_images
            .len(),
        1
    );
    let after = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(after.config, before.config);
}

#[tokio::test]
async fn known_bad_image_add_and_remove_race_on_absent_hash_is_serializable() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let write = ConfigurationWriteScope::new(scope.clone(), scope).unwrap();
    let hash = "fe".repeat(32);
    let (add_res, remove_res) = tokio::join!(
        add_enrolled_known_bad_image(&f.db, write.clone(), &hash, None),
        remove_enrolled_known_bad_image(&f.db, write.clone(), &hash),
    );
    assert!(add_res.is_ok());
    assert!(remove_res.is_ok());
    let add = add_res.unwrap();
    let remove = remove_res.unwrap();
    let rows = load_config_parts(&f.db, 10).await.unwrap().known_bad_images;
    let present = rows.iter().any(|row| row.hash == hash);
    if remove && present {
        panic!("remove succeeded while the hash remained listed");
    }
    match (remove, add) {
        (false, true) => {
            assert!(present);
            assert_eq!(rows.len(), 1);
        }
        (true, false) => assert!(!present),
        (true, true) => assert!(!present),
        _ => panic!("unexpected add/remove pair: add={add}, remove={remove}"),
    }
}

#[tokio::test]
async fn atomic_global_rule_save_preserves_both_rows_on_fence_membership_or_commit_failure() {
    for failure in ["fence", "membership", "commit"] {
        let f = Fixture::new();
        let request = f.scope(10).await;
        let original: RuleDef =
            serde_json::from_value(serde_json::json!({"name":"original", "description":""}))
                .unwrap();
        upsert_rule(&f.db, 0, "mention_spam", &original)
            .await
            .unwrap();
        upsert_rule(&f.db, 10, "mention_spam", &original)
            .await
            .unwrap();
        if failure == "fence" {
            let store = crate::db::guild_stores::StoreId::new();
            crate::db::guild_stores::register_store(
                &f.db,
                store.clone(),
                crate::db::guild_stores::StorePolicy::Dedicated { guild_id: 10 },
            )
            .await
            .unwrap();
            f.db.with_conn(move |conn| {
                conn.execute_bounded(
                    "INSERT INTO guild_write_fences(guild_id,generation,phase,destination_store_id,destination_revision,created_at)
                     VALUES(10,?1,'freeze',?2,?1,unixepoch())",
                    params![format!("{:032x}", 9u128), store.as_str()], 4096,
                )?;
                Ok(())
            }).await.unwrap();
        } else if failure == "membership" {
            retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
            enroll_guild(&f.db, 10, 101).await.unwrap();
            // Retirement erased the old membership's rows; protect a replacement override.
            upsert_rule(&f.db, 10, "mention_spam", &original)
                .await
                .unwrap();
        } else {
            f.db.with_conn(|conn| conn.execute_batch_bounded(
                "CREATE TRIGGER refuse_rule_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'fixture'); END", 4096
            )).await.unwrap();
        }
        let before = get_primary_versions(&f.db).await.unwrap();
        let mut changed = original;
        changed.name = "must roll back".into();
        assert!(
            upsert_global_rule_and_clear_enrolled_override(
                &f.db,
                request,
                "mention_spam",
                &changed
            )
            .await
            .is_err(),
            "{failure}"
        );
        for guild in [0, 10] {
            assert_eq!(
                get_rule(&f.db, guild, "mention_spam")
                    .await
                    .unwrap()
                    .unwrap()
                    .name,
                "original",
                "{failure}: guild {guild}"
            );
        }
        assert_eq!(
            get_primary_versions(&f.db).await.unwrap(),
            before,
            "{failure}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_command_saves_preserve_versions_across_concurrency_and_restart() {
    let f = Fixture::new();
    let scope = f.scope(10).await;
    let write = edit_scope(&scope, false);
    let mut command = crate::defaults::sample_plan_command();
    upsert_enrolled_command(&f.db, write.clone(), &command.name, &command)
        .await
        .unwrap();
    let before = get_primary_versions(&f.db).await.unwrap();
    upsert_enrolled_command(&f.db, write.clone(), &command.name, &command)
        .await
        .unwrap();
    assert_eq!(get_primary_versions(&f.db).await.unwrap(), before);

    command.description.push_str(" changed");
    let (first, second) = tokio::join!(
        upsert_enrolled_command(&f.db, write.clone(), &command.name, &command),
        upsert_enrolled_command(&f.db, write.clone(), &command.name, &command),
    );
    first.unwrap();
    second.unwrap();
    let changed = get_primary_versions(&f.db).await.unwrap();
    assert_eq!(changed.config, before.config + 1);
    assert_eq!(changed.slash, before.slash + 1);
    assert_ne!(changed.config_revision, before.config_revision);
    assert_ne!(changed.slash_revision, before.slash_revision);

    let reopened = Db::open(&f.path, 2).unwrap();
    upsert_enrolled_command(&reopened, write.clone(), &command.name, &command)
        .await
        .unwrap();
    assert_eq!(get_primary_versions(&reopened).await.unwrap(), changed);

    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    let retired = get_primary_versions(&f.db).await.unwrap();
    let error = upsert_enrolled_command(&f.db, write, &command.name, &command)
        .await
        .unwrap_err();
    assert!(error.is::<EnrollmentChanged>(), "{error:#}");
    assert_eq!(get_primary_versions(&f.db).await.unwrap(), retired);
}
