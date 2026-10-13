use super::*;

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("configuration_reads");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn scope(&self) -> ConfigurationScope {
        ConfigurationScope::Guild(enroll_guild(&self.db, 10, 100).await.unwrap().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}
async fn reads(db: &Db, scope: ConfigurationScope) -> Vec<Result<()>> {
    vec![
        get_enrolled_rule(db, scope.clone(), "absent")
            .await
            .map(|_| ()),
        list_enrolled_commands(db, scope.clone()).await.map(|_| ()),
        get_enrolled_command(db, scope.clone(), "absent")
            .await
            .map(|_| ()),
        list_enrolled_templates(db, scope.clone()).await.map(|_| ()),
        get_enrolled_template(db, scope.clone(), "absent")
            .await
            .map(|_| ()),
        list_enrolled_badwords(db, scope.clone()).await.map(|_| ()),
        list_enrolled_log_channels(db, scope).await.map(|_| ()),
    ]
}
fn same_json(a: impl serde::Serialize, b: impl serde::Serialize) {
    assert_eq!(
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap()
    );
}

#[tokio::test]
async fn configuration_reads_reject_stale_and_forged_scopes_even_for_missing_rows() {
    let f = Fixture::new();
    let scope = f.scope().await;
    let ConfigurationScope::Guild(original) = &scope else {
        unreachable!()
    };
    for changed in [
        GuildEnrollment {
            guild_id: 0,
            ..original.clone()
        },
        GuildEnrollment {
            guild_id: 20,
            ..original.clone()
        },
        GuildEnrollment {
            id: "forged".into(),
            ..original.clone()
        },
        GuildEnrollment {
            joined_at_ns: 101,
            ..original.clone()
        },
    ] {
        for result in reads(&f.db, ConfigurationScope::Guild(changed)).await {
            assert!(result.unwrap_err().is::<EnrollmentChanged>());
        }
    }
    for result in reads(&f.db, scope.clone()).await {
        result.unwrap();
    }
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    for result in reads(&f.db, scope.clone()).await {
        assert!(result.unwrap_err().is::<EnrollmentChanged>());
    }
    let current = ConfigurationScope::Guild(enroll_guild(&f.db, 10, 101).await.unwrap().unwrap());
    for result in reads(&f.db, scope).await {
        assert!(result.unwrap_err().is::<EnrollmentChanged>());
    }
    for result in reads(&f.db, current).await {
        result.unwrap();
    }
}

#[tokio::test]
async fn configuration_layers_preserve_global_guild_order_and_exact_key_semantics() {
    let f = Fixture::new();
    let scope = f.scope().await;
    for (guild, label) in [(0, "global"), (10, "guild"), (20, "foreign")] {
        let mut rule = crate::defaults::default_rules()[0].clone();
        rule.enabled = guild != 0;
        upsert_rule(&f.db, guild, "fixture", &rule).await.unwrap();
        let mut command = crate::defaults::default_commands()[0].clone();
        command.name = "fixture".into();
        command.description = label.into();
        upsert_command(&f.db, guild, "fixture", &command)
            .await
            .unwrap();
        upsert_template(&f.db, guild, "fixture", label)
            .await
            .unwrap();
        add_badword(&f.db, guild, label, Some(label)).await.unwrap();
        upsert_log_channel(&f.db, guild, "fixture", 9_007_199_254_740_993 + guild)
            .await
            .unwrap();
    }
    // Global rows are returned as both layers for the owner global context, exactly
    // as before; a guild context includes only that guild plus the global layer.
    for (captured, guild) in [(scope.clone(), 10), (ConfigurationScope::Global, 0)] {
        let rule = get_enrolled_rule(&f.db, captured.clone(), "fixture")
            .await
            .unwrap();
        same_json(rule.guild, get_rule(&f.db, guild, "fixture").await.unwrap());
        same_json(rule.global, get_rule(&f.db, 0, "fixture").await.unwrap());
        let commands = list_enrolled_commands(&f.db, captured.clone())
            .await
            .unwrap();
        same_json(commands.guild, list_commands(&f.db, guild).await.unwrap());
        same_json(commands.global, list_commands(&f.db, 0).await.unwrap());
        let command = get_enrolled_command(&f.db, captured.clone(), "fixture")
            .await
            .unwrap();
        same_json(
            command.guild,
            get_command_row(&f.db, guild, "fixture").await.unwrap(),
        );
        same_json(
            command.global,
            get_command_row(&f.db, 0, "fixture").await.unwrap(),
        );
        let templates = list_enrolled_templates(&f.db, captured.clone())
            .await
            .unwrap();
        assert_eq!(templates.guild, list_templates(&f.db, guild).await.unwrap());
        assert_eq!(templates.global, list_templates(&f.db, 0).await.unwrap());
        let template = get_enrolled_template(&f.db, captured.clone(), "fixture")
            .await
            .unwrap();
        assert_eq!(
            template.guild,
            get_template_row(&f.db, guild, "fixture").await.unwrap()
        );
        assert_eq!(
            template.global,
            get_template_row(&f.db, 0, "fixture").await.unwrap()
        );
        let words = list_enrolled_badwords(&f.db, captured.clone())
            .await
            .unwrap();
        same_json(
            words.iter().map(|w| w.with_status()).collect::<Vec<_>>(),
            list_badwords(&f.db, guild)
                .await
                .unwrap()
                .iter()
                .map(|w| w.with_status())
                .collect::<Vec<_>>(),
        );
        assert!(words.iter().all(|w| w.guild_id == guild || w.guild_id == 0));
        let channels = list_enrolled_log_channels(&f.db, captured).await.unwrap();
        assert_eq!(channels, list_log_channels(&f.db, guild).await.unwrap());
        assert_eq!(
            channels.iter().find(|(e, _)| e == "fixture").unwrap().1,
            9_007_199_254_740_993 + guild
        );
    }
    let missing = get_enrolled_command(&f.db, scope.clone(), "' OR 1=1 --")
        .await
        .unwrap();
    assert!(missing.guild.is_none() && missing.global.is_none());
    let missing = get_enrolled_template(&f.db, scope.clone(), "absent")
        .await
        .unwrap();
    assert!(missing.guild.is_none() && missing.global.is_none());
    // Detail reads must not load/decode unrelated settings or other configuration rows.
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "INSERT INTO rules(guild_id,rule_id,data) VALUES(10,'corrupt','not json')",
            64 * 1024 * 1024,
        )
    })
    .await
    .unwrap();
    get_enrolled_rule(&f.db, scope.clone(), "fixture")
        .await
        .unwrap();
    get_enrolled_command(&f.db, scope, "fixture").await.unwrap();
}

#[tokio::test]
async fn configuration_reads_preserve_versions_and_refuse_unavailable_membership_or_low_floor() {
    let f = Fixture::new();
    let scope = f.scope().await;
    let before = get_primary_versions(&f.db).await.unwrap();
    f.db.with_conn(|conn| conn.execute_batch_bounded("CREATE TRIGGER reject_config_read_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'read wrote receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    for captured in [scope.clone(), ConfigurationScope::Global] {
        for result in reads(&f.db, captured).await {
            result.unwrap();
        }
    }
    assert_eq!(get_primary_versions(&f.db).await.unwrap(), before);
    f.db.require_config_version(get_config_version(&f.db).await.unwrap() + 1);
    for result in reads(&f.db, scope.clone()).await {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("committed-version floor")
        );
    }
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "ALTER TABLE guild_enrollments RENAME TO unavailable_enrollments",
            64 * 1024 * 1024,
        )
    })
    .await
    .unwrap();
    for result in reads(&f.db, scope).await {
        assert!(result.unwrap_err().is::<EnrollmentUnverified>());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configuration_read_keeps_prior_membership_without_waiting_for_the_writer() {
    let f = Fixture::new();
    let scope = f.scope().await;
    let writer_db = f.db.clone();
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let writer = tokio::spawn(async move {
        writer_db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute_bounded("UPDATE guild_enrollments SET id='replacement',joined_at_ns=101 WHERE guild_id=10", params![], 64 * 1024 * 1024)?;
            locked.send(()).unwrap();
            wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            tx.commit()
        }).await.unwrap();
    });
    ready.await.unwrap();
    let read = get_enrolled_template(&f.db, scope.clone(), "absent");
    tokio::pin!(read);
    let during = tokio::time::timeout(std::time::Duration::from_millis(500), &mut read)
        .await
        .expect("a primary snapshot must finish while a writer holds the lock");
    assert!(
        !during
            .as_ref()
            .is_err_and(|error| error.is::<EnrollmentChanged>()),
        "the snapshot must still show the membership committed before the writer: {during:?}"
    );
    release.send(()).unwrap();
    writer.await.unwrap();
    assert!(
        get_enrolled_template(&f.db, scope, "absent")
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
}

#[tokio::test]
async fn configuration_layers_are_coherent_during_concurrent_updates() {
    let f = Fixture::new();
    let scope = f.scope().await;
    for g in [0, 10] {
        upsert_template(&f.db, g, "paired", "0").await.unwrap();
    }
    let writer_db = f.db.clone();
    let writer = tokio::spawn(async move {
        for n in 1..=40 {
            writer_db.with_conn(move |conn| {
                let tx = conn.transaction_immediate()?;
                tx.execute_bounded("UPDATE templates SET template=?1 WHERE event_type='paired' AND guild_id IN (0,10)", params![n.to_string()], 64 * 1024 * 1024)?;
                tx.commit()
            }).await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    for _ in 0..40 {
        let pair = get_enrolled_template(&f.db, scope.clone(), "paired")
            .await
            .unwrap();
        assert_eq!(
            pair.guild, pair.global,
            "read mixed two committed generations"
        );
    }
    writer.await.unwrap();
}

#[tokio::test]
async fn web_config_snapshot_matches_layer_helpers_without_receipts() {
    let f = Fixture::new();
    let scope = f.scope().await;
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "CREATE TRIGGER reject_config_read_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'read wrote receipt'); END", 64 * 1024 * 1024,
        )
    })
    .await
    .unwrap();
    let mut rule = crate::defaults::default_rules()[0].clone();
    rule.enabled = false;
    upsert_rule(&f.db, 0, "invite", &rule).await.unwrap();
    rule.enabled = true;
    upsert_rule(&f.db, 10, "invite", &rule).await.unwrap();
    let mut command = crate::defaults::default_commands()[0].clone();
    command.name = "snapshot".into();
    command.description = "global".into();
    upsert_command(&f.db, 0, "snapshot", &command)
        .await
        .unwrap();
    command.description = "guild".into();
    upsert_command(&f.db, 10, "snapshot", &command)
        .await
        .unwrap();
    upsert_template(&f.db, 0, "warn", "global-warn")
        .await
        .unwrap();
    upsert_template(&f.db, 10, "warn", "guild-warn")
        .await
        .unwrap();
    add_badword(&f.db, 0, "global-word", Some("g"))
        .await
        .unwrap();
    add_badword(&f.db, 10, "guild-word", Some("l"))
        .await
        .unwrap();
    upsert_log_channel(&f.db, 10, "warn", 42).await.unwrap();
    patch_settings(&f.db, 10, Some(Some("?".into())), None, None)
        .await
        .unwrap();

    let snapshot = load_enrolled_web_config_snapshot(&f.db, scope.clone())
        .await
        .unwrap();
    let settings = get_settings_web_bundle(&f.db, 10).await.unwrap();
    assert_eq!(snapshot.settings.settings.prefix, "?");
    assert_eq!(snapshot.settings.settings.prefix, settings.settings.prefix);
    assert_eq!(
        snapshot.settings.settings_override,
        settings.settings_override
    );
    assert_eq!(
        snapshot.settings.global_settings.prefix,
        settings.global_settings.prefix
    );
    same_json(
        snapshot.guild_rules.get("invite"),
        get_rule(&f.db, 10, "invite").await.unwrap().as_ref(),
    );
    same_json(
        snapshot.global_rules.get("invite"),
        get_rule(&f.db, 0, "invite").await.unwrap().as_ref(),
    );
    same_json(
        snapshot.guild_commands.get("snapshot"),
        get_command_row(&f.db, 10, "snapshot")
            .await
            .unwrap()
            .as_ref(),
    );
    same_json(
        snapshot.global_commands.get("snapshot"),
        get_command_row(&f.db, 0, "snapshot")
            .await
            .unwrap()
            .as_ref(),
    );
    assert_eq!(
        snapshot.guild_templates.get("warn").map(String::as_str),
        Some("guild-warn")
    );
    assert_eq!(
        snapshot.global_templates.get("warn").map(String::as_str),
        Some("global-warn")
    );
    assert_eq!(snapshot.guild_log_channels.get("warn").copied(), Some(42));
    assert_eq!(
        snapshot
            .badwords
            .iter()
            .map(|w| (w.guild_id, w.pattern.as_str(), w.note.as_deref()))
            .collect::<Vec<_>>(),
        vec![(0, "global-word", Some("g")), (10, "guild-word", Some("l"))]
    );
    let (version, _, parts) = load_config_parts_with_version(&f.db, 10).await.unwrap();
    assert_eq!(snapshot.version, version);
    assert_eq!(parts.settings.prefix, "?");
    assert!(parts.rules["invite"].enabled);
    assert_eq!(parts.commands["snapshot"].description, "guild");
    assert_eq!(parts.templates["warn"], "guild-warn");
    assert_eq!(parts.log_channels["warn"], 42);
    assert_eq!(
        parts.badwords,
        vec!["global-word".to_string(), "guild-word".to_string()]
    );
}
