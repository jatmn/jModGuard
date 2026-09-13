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
        conn.execute_batch("ALTER TABLE guild_enrollments RENAME TO test_hidden_table")
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
    f.db.with_conn(|conn|conn.execute_batch("CREATE TRIGGER refuse_version BEFORE UPDATE ON meta WHEN NEW.key='config_version' BEGIN SELECT RAISE(ABORT,'fixture'); END")).await.unwrap();
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
        conn.execute("INSERT INTO badwords(guild_id,pattern) VALUES(10,'[')", ())?;
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
