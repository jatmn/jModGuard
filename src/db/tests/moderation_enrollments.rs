use super::*;

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("moderation_scope");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}
async fn denied(db: &Db, enrollment: &GuildEnrollment) {
    assert!(
        query_enrolled_events(db, enrollment.clone(), None, None, 50)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        list_enrolled_bans(db, enrollment.clone(), 100)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    assert!(
        enrolled_guild_stats(db, enrollment.clone())
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
}

#[tokio::test]
async fn moderation_reads_refuse_retired_replaced_and_modified_scopes_even_when_empty() {
    let f = Fixture::new();
    let original = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
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
        denied(&f.db, &changed).await;
    }
    assert!(
        query_enrolled_events(&f.db, original.clone(), None, None, 50)
            .await
            .unwrap()
            .is_empty()
    );
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    denied(&f.db, &original).await;
    let current = enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
    denied(&f.db, &original).await;
    assert_eq!(
        enrolled_guild_stats(&f.db, current).await.unwrap().events,
        0
    );
}

#[tokio::test]
async fn moderation_scopes_preserve_filter_order_limits_wire_data_and_guild_isolation() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    f.db.with_conn(|conn| {
        let tx = conn.transaction_immediate()?;
        for i in 0..205i64 {
            tx.execute_bounded("INSERT INTO events(guild_id,event_type,user_id,data,created_at) VALUES(10,'member_ban',?1,'{\"snowflake\":1329331947876479006}','now')", params![i], 64 * 1024 * 1024)?;
            tx.execute_bounded("INSERT INTO bans(guild_id,user_id,reason,created_at,active) VALUES(10,?1,'fixture','now',?2)", params![i, i % 2], 64 * 1024 * 1024)?;
        }
        tx.commit()
    }).await.unwrap();
    insert_event(
        &f.db,
        20,
        "member_ban",
        Some(5),
        None,
        &serde_json::json!({"foreign":"secret"}),
    )
    .await
    .unwrap();
    record_ban(&f.db, 20, 999, None, "foreign secret")
        .await
        .unwrap();
    for limit in [-1, 2, 1000] {
        let events = query_enrolled_events(&f.db, scope.clone(), None, None, limit)
            .await
            .unwrap();
        let bans = list_enrolled_bans(&f.db, scope.clone(), limit)
            .await
            .unwrap();
        assert_eq!(events.len(), limit.clamp(1, 200) as usize);
        assert_eq!(bans.len(), limit.clamp(1, 200) as usize);
        assert!(events.iter().all(|e| e.guild_id == 10));
        assert!(bans.iter().all(|b| b.guild_id == 10));
        assert_eq!(events[0].user_id, Some(204));
        assert_eq!(bans[0].user_id, 204);
        let data: serde_json::Value =
            serde_json::from_str(events[0].data.as_ref().unwrap()).unwrap();
        assert_eq!(data["snowflake"], "1329331947876479006");
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            serde_json::to_value(query_events(&f.db, 10, None, None, limit).await.unwrap())
                .unwrap()
        );
        assert_eq!(
            serde_json::to_value(bans).unwrap(),
            serde_json::to_value(list_bans(&f.db, 10, limit).await.unwrap()).unwrap()
        );
    }
    // Sparse and absent users must retain their filters with or without a type.
    // The matching row is near the start of the guild history; another guild
    // has a newer row for the same user.
    for event_type in [None, Some("member_ban")] {
        for (user, count) in [(5, 1), (999, 0)] {
            let rows = query_enrolled_events(&f.db, scope.clone(), event_type, Some(user), 50)
                .await
                .unwrap();
            assert_eq!(rows.len(), count);
            assert!(
                rows.iter()
                    .all(|row| row.guild_id == 10 && row.user_id == Some(user))
            );
            assert_eq!(
                serde_json::to_value(&rows).unwrap(),
                serde_json::to_value(
                    query_events(&f.db, 10, event_type, Some(user), 50)
                        .await
                        .unwrap()
                )
                .unwrap()
            );
        }
    }
    assert!(
        query_enrolled_events(&f.db, scope.clone(), Some("member_kick"), None, 50)
            .await
            .unwrap()
            .is_empty()
    );
    let stats = enrolled_guild_stats(&f.db, scope.clone()).await.unwrap();
    assert_eq!((stats.events, stats.bans, stats.warns), (205, 205, 0));
    assert_eq!(
        serde_json::to_value(stats).unwrap(),
        serde_json::to_value(guild_stats(&f.db, 10).await.unwrap()).unwrap()
    );
    // This boundary does not purge historical rows or assign them to an enrollment.
    let current = enroll_guild(&f.db, 10, 101).await.unwrap().unwrap();
    denied(&f.db, &scope).await;
    assert_eq!(
        list_enrolled_bans(&f.db, current, 1).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn gateway_ban_identity_survives_scoped_read_and_reopen() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let user_id = 1_726_972_779_646_156_581i64;
    record_enrolled_ban_with_identity(
        &f.db,
        Some(scope.clone()),
        user_id,
        Some("wavingwalker"),
        Some("Wavingwalker"),
        Some(1_800_882_749_273_538_599),
        Some("ikaika44"),
        "No reason provided",
        Some("2026-09-22T23:16:28Z"),
    )
    .await
    .unwrap();
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_name.as_deref(), Some("wavingwalker"));
    assert_eq!(rows[0].user_display_name.as_deref(), Some("Wavingwalker"));
    assert_eq!(rows[0].moderator_name.as_deref(), Some("ikaika44"));
    assert_eq!(rows[0].created_at, "2026-09-22T23:16:28Z");
    let json = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(json["user_id"], user_id.to_string());
    assert_eq!(json["moderator_id"], "1800882749273538599");
    assert_eq!(json["reason"], "No reason provided");
    let api = Db::open_with(DbOpenOptions {
        read_only: false,
        path: f.path.clone(),
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 7,
    })
    .unwrap();
    assert_eq!(
        list_bans(&api, 10, 10).await.unwrap()[0]
            .user_name
            .as_deref(),
        Some("wavingwalker")
    );
    let later_id = record_enrolled_ban_with_identity(
        &f.db,
        Some(scope.clone()),
        user_id + 1,
        Some("other-user"),
        None,
        None,
        None,
        "Reason unavailable",
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        list_bans(&api, 10, 10).await.unwrap()[0].reason.as_deref(),
        Some("Reason unavailable")
    );
    mark_enrolled_unbanned(&f.db, Some(scope.clone()), user_id + 1)
        .await
        .unwrap();
    attribute_enrolled_ban(
        &f.db,
        Some(scope.clone()),
        later_id,
        user_id + 1,
        777,
        Some("late-moderator"),
        "late audit reason",
    )
    .await
    .unwrap();
    let later = list_bans(&api, 10, 10).await.unwrap();
    assert_eq!(later[0].moderator_id, Some(777));
    assert!(!later[0].active, "a late audit must not undo an unban");
    assert_eq!(later[0].moderator_name.as_deref(), Some("late-moderator"));
    assert_eq!(later[0].reason.as_deref(), Some("late audit reason"));
    assert_eq!(later[1].moderator_name.as_deref(), Some("ikaika44"));
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    enroll_guild(&f.db, 10, 101).await.unwrap();
    assert!(
        attribute_enrolled_ban(
            &f.db,
            Some(scope),
            later_id,
            user_id + 1,
            778,
            None,
            "wrong membership",
        )
        .await
        .unwrap_err()
        .is::<EnrollmentChanged>()
    );
}

#[tokio::test]
async fn moderation_reads_create_no_receipts_or_version_changes_and_fail_closed() {
    let probe = Arc::new(crate::db_compat::StartupProbe::default());
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let version = get_config_version(&f.db).await.unwrap();
    f.db.with_conn(|conn| conn.execute_batch_bounded("CREATE TRIGGER reject_read_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'read wrote receipt'); END", 64 * 1024 * 1024)).await.unwrap();
    let before = probe.immediate();
    guild_stats(&f.db, 10).await.unwrap();
    query_enrolled_events(&f.db, scope.clone(), None, None, 50)
        .await
        .unwrap();
    list_enrolled_bans(&f.db, scope.clone(), 100).await.unwrap();
    enrolled_guild_stats(&f.db, scope.clone()).await.unwrap();
    assert_eq!(get_config_version(&f.db).await.unwrap(), version);
    assert_eq!(
        probe.immediate(),
        before,
        "bot and dashboard reads must not reserve the writer"
    );
    f.db.with_conn(|conn| {
        conn.execute_batch_bounded(
            "ALTER TABLE guild_enrollments RENAME TO unavailable_enrollments",
            64 * 1024 * 1024,
        )
    })
    .await
    .unwrap();
    for error in [
        query_enrolled_events(&f.db, scope.clone(), None, None, 50)
            .await
            .unwrap_err(),
        list_enrolled_bans(&f.db, scope.clone(), 100)
            .await
            .unwrap_err(),
        enrolled_guild_stats(&f.db, scope).await.unwrap_err(),
    ] {
        assert!(error.is::<EnrollmentUnverified>());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moderation_read_keeps_prior_membership_without_waiting_for_the_writer() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
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
    let read = enrolled_guild_stats(&f.db, scope.clone());
    tokio::pin!(read);
    tokio::time::timeout(std::time::Duration::from_millis(500), &mut read)
        .await
        .expect("a primary snapshot must finish while a writer holds the lock")
        .expect("the snapshot still shows the membership committed before the writer");
    release.send(()).unwrap();
    writer.await.unwrap();
    assert!(
        enrolled_guild_stats(&f.db, scope)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
}

#[tokio::test]
async fn stale_active_ban_is_replaced_and_the_same_dispatch_is_not_inserted_twice() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    record_enrolled_ban_with_identity(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(1),
        None,
        "old reason",
        Some("2020-01-01T00:00:00+00:00"),
    )
    .await
    .unwrap();
    let occurred = "2024-02-02T00:00:00+00:00";
    let id = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "new reason",
        Some(occurred),
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert!(id.is_some());
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, id.unwrap());
    assert_eq!(active[0].reason.as_deref(), Some("new reason"));
    assert_eq!(active[0].moderator_id, Some(9));
    assert!(
        rows.iter()
            .any(|row| !row.active && row.reason.as_deref() == Some("old reason"))
    );
    let again = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "new reason",
        Some(occurred),
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert_eq!(again, None);
    assert_eq!(
        list_enrolled_bans(&f.db, scope.clone(), 10)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.active)
            .count(),
        1
    );
}

#[tokio::test]
async fn same_dispatch_without_occurred_at_inserts_once() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "handoff",
        None,
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert!(first.is_some());
    let second = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "handoff",
        None,
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert_eq!(second, None);
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, first.unwrap());
    assert_eq!(active[0].reason.as_deref(), Some("handoff"));
    assert_eq!(active[0].moderator_id, Some(9));
}

#[tokio::test]
async fn ended_ban_does_not_block_a_new_insert() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    record_enrolled_ban_with_identity(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(1),
        None,
        "ended",
        Some("2020-01-01T00:00:00+00:00"),
    )
    .await
    .unwrap();
    mark_enrolled_unbanned(&f.db, Some(scope.clone()), 55)
        .await
        .unwrap();
    let id = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "fresh",
        Some("2024-03-03T00:00:00+00:00"),
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert!(id.is_some());
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    assert_eq!(
        rows.iter()
            .find(|row| row.active)
            .unwrap()
            .reason
            .as_deref(),
        Some("fresh")
    );
}

#[tokio::test]
async fn fresh_ban_replaces_a_matching_active_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(first.is_some());
    let replacement = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(replacement.is_some());
    assert_ne!(replacement, first);
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, replacement.unwrap());
    assert_eq!(rows.iter().filter(|row| !row.active).count(), 1);
}

#[tokio::test]
async fn missed_unban_then_the_same_automod_reban_inserts_a_new_active_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let reason = "[automod:invite] posted a link";
    let first = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        None,
        None,
        reason,
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(first.is_some());
    let replacement = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        None,
        None,
        reason,
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(replacement.is_some());
    assert_ne!(replacement, first);
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, replacement.unwrap());
    assert_eq!(active[0].reason.as_deref(), Some(reason));
    assert_eq!(active[0].moderator_id, None);
    assert_eq!(rows.iter().filter(|row| !row.active).count(), 1);
}

#[tokio::test]
async fn unban_then_reban_with_the_same_reason_and_moderator_inserts() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(first.is_some());
    mark_enrolled_unbanned(&f.db, Some(scope.clone()), 55)
        .await
        .unwrap();
    let again = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(again.is_some());
    assert_ne!(again, first);
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, again.unwrap());
    assert_eq!(active[0].reason.as_deref(), Some("spam"));
    assert_eq!(active[0].moderator_id, Some(9));
}

#[tokio::test]
async fn retry_of_the_same_ban_dispatch_inserts_once() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    let again = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Retry,
        None,
    )
    .await
    .unwrap();
    assert!(first.is_some());
    assert_eq!(again, None);
    assert_eq!(
        list_enrolled_bans(&f.db, scope, 10)
            .await
            .unwrap()
            .iter()
            .filter(|row| row.active)
            .count(),
        1
    );
}

fn member_ban_events(rows: &[EventRow]) -> usize {
    rows.iter()
        .filter(|row| row.event_type == "member_ban")
        .count()
}

#[tokio::test]
async fn manual_discord_ban_is_recorded_once() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let recorded = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        Some("Member"),
        Some("2024-04-04T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id } = recorded else {
        panic!("a manual ban must insert, got {recorded:?}");
    };
    let again = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        Some("Member"),
        Some("2024-04-04T00:00:01+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(again, GatewayBanWrite::Existing { row_id });
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].active);
    assert_eq!(rows[0].id, row_id);
    assert_eq!(rows[0].moderator_id, None);
    assert_eq!(rows[0].reason.as_deref(), Some(GATEWAY_BAN_REASON));
    let events = query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), Some(55), 50)
        .await
        .unwrap();
    assert_eq!(member_ban_events(&events), 1);
    let stats = enrolled_guild_stats(&f.db, scope).await.unwrap();
    assert_eq!((stats.bans, stats.events), (1, 1));
}

#[tokio::test]
async fn duplicate_gateway_ban_events_leave_one_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let second = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id } = first else {
        panic!("first delivery must insert, got {first:?}");
    };
    assert_eq!(second, GatewayBanWrite::Existing { row_id });
    assert_eq!(
        list_enrolled_bans(&f.db, scope.clone(), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        1
    );
}

#[tokio::test]
async fn bot_ban_is_not_recorded_again_when_the_gateway_echo_arrives() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let inserted = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "spam",
        Some("2024-04-04T00:00:00+00:00"),
        BanRecord::Fresh,
        None,
    )
    .await
    .unwrap();
    assert!(inserted.is_some());
    let echo = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some("2024-04-04T00:00:01+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        echo,
        GatewayBanWrite::Existing {
            row_id: inserted.unwrap()
        }
    );
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason.as_deref(), Some("spam"));
    assert_eq!(rows[0].moderator_id, Some(9));
    assert_eq!(rows[0].moderator_name.as_deref(), Some("bot"));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
}

#[tokio::test]
async fn gateway_echo_before_the_command_still_leaves_the_bot_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let recovered = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        true,
        Some("attempt-same"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id } = recovered else {
        panic!("a bot echo with no row must store the mirror, got {recovered:?}");
    };
    assert_eq!(
        list_enrolled_bans(&f.db, scope.clone(), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
    let inserted = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        None,
        "spam",
        None,
        BanRecord::Fresh,
        Some("attempt-same"),
    )
    .await
    .unwrap();
    assert_eq!(inserted, Some(row_id));
    let echo = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(echo, GatewayBanWrite::Existing { row_id });
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason.as_deref(), Some("spam"));
    assert_eq!(rows[0].moderator_id, Some(9));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
}

#[tokio::test]
async fn manual_reban_after_unban_inserts_a_new_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    mark_enrolled_unbanned(&f.db, Some(scope.clone()), 55)
        .await
        .unwrap();
    let second = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: first_id } = first else {
        panic!("first ban must insert, got {first:?}");
    };
    let GatewayBanWrite::Recorded { row_id: second_id } = second else {
        panic!("reban must insert, got {second:?}");
    };
    assert_ne!(first_id, second_id);
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        2
    );
}

#[tokio::test]
async fn database_guard_rejects_a_second_active_ban_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let error = record_enrolled_ban_with_identity(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        None,
        "second active",
        None,
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("UNIQUE constraint failed") || text.contains("idx_bans_one_active"),
        "{text}"
    );
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].active);
    assert_eq!(rows[0].reason.as_deref(), Some(GATEWAY_BAN_REASON));
}

#[tokio::test]
async fn uncertain_bot_ban_echo_inserts_one_active_row() {
    // An uncertain Discord result keeps the bot's suppression and does not
    // insert from the command. The echo is still the bot's, and it is the
    // only chance to store the ban Discord already applied.
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let write = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some("2024-04-04T00:00:00+00:00"),
        true,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id } = write else {
        panic!("uncertain echo must store the missing row, got {write:?}");
    };
    let again = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        true,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(again, GatewayBanWrite::Existing { row_id });
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].active);
    assert_eq!(rows[0].id, row_id);
    assert_eq!(rows[0].moderator_id, None);
    assert_eq!(rows[0].reason.as_deref(), Some(GATEWAY_BAN_REASON));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
    assert_eq!(enrolled_guild_stats(&f.db, scope).await.unwrap().bans, 1);
}

#[tokio::test]
async fn failed_bot_mirror_echo_inserts_one_active_row() {
    // Discord accepted the ban, but the command's mirror write did not commit.
    // The owned echo stores the one active row and does not add a member_ban
    // event. A later command write fills that same row instead of adding one.
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let write = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        None,
        true,
        Some("attempt-same"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id } = write else {
        panic!("echo after a failed mirror must store the row, got {write:?}");
    };
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
    let filled = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "spam",
        None,
        BanRecord::Retry,
        Some("attempt-same"),
    )
    .await
    .unwrap();
    assert_eq!(filled, Some(row_id));
    let again = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "spam",
        None,
        BanRecord::Retry,
        Some("attempt-same"),
    )
    .await
    .unwrap();
    assert_eq!(again, None);
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].active);
    assert_eq!(rows[0].id, row_id);
    assert_eq!(rows[0].reason.as_deref(), Some("spam"));
    assert_eq!(rows[0].moderator_id, Some(9));
    assert_eq!(rows[0].moderator_name.as_deref(), Some("bot"));
    assert_eq!(rows[0].user_name.as_deref(), Some("member"));
    let echo = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        None,
        true,
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(echo, GatewayBanWrite::Existing { row_id });
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
}

#[tokio::test]
async fn fresh_bot_ban_ends_an_older_manual_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let created = "2024-01-01T00:00:00+00:00";
    let manual = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some(created),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: old_id } = manual else {
        panic!("manual ban must insert, got {manual:?}");
    };
    let inserted = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "spam",
        Some("2024-06-01T00:00:00+00:00"),
        BanRecord::Fresh,
        Some("bot-attempt-new"),
    )
    .await
    .unwrap();
    let new_id = inserted.expect("a new ban inserts its own row");
    assert_ne!(new_id, old_id);
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, new_id);
    assert_eq!(active[0].moderator_id, Some(9));
    assert_eq!(active[0].reason.as_deref(), Some("spam"));
    let ended = rows.iter().find(|row| row.id == old_id).unwrap();
    assert!(!ended.active);
    assert_eq!(ended.created_at, created);
    assert_eq!(ended.moderator_id, None);
    assert_eq!(ended.reason.as_deref(), Some(GATEWAY_BAN_REASON));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        1
    );
}

#[tokio::test]
async fn fresh_bot_ban_ends_an_older_uncertain_echo_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let created = "2024-02-02T00:00:00+00:00";
    let echo = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some(created),
        true,
        Some("echo-attempt-old"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id: old_id } = echo else {
        panic!("uncertain echo must store its own row, got {echo:?}");
    };
    let inserted = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "raid",
        Some("2024-06-02T00:00:00+00:00"),
        BanRecord::Fresh,
        Some("bot-attempt-new"),
    )
    .await
    .unwrap();
    let new_id = inserted.expect("a distinct ban inserts its own row");
    assert_ne!(new_id, old_id);
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    let active: Vec<_> = rows.iter().filter(|row| row.active).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, new_id);
    assert_eq!(active[0].moderator_id, Some(9));
    assert_eq!(active[0].reason.as_deref(), Some("raid"));
    let ended = rows.iter().find(|row| row.id == old_id).unwrap();
    assert!(!ended.active);
    assert_eq!(ended.created_at, created);
    assert_eq!(ended.moderator_id, None);
    assert_eq!(ended.reason.as_deref(), Some(GATEWAY_BAN_REASON));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
}

#[tokio::test]
async fn same_attempt_echo_then_command_keeps_one_bot_row() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let echo = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        None,
        true,
        Some("bot-attempt-same"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id } = echo else {
        panic!("the echo must store the attempt, got {echo:?}");
    };
    let filled = record_enrolled_ban_if_absent(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some(9),
        Some("bot"),
        "spam",
        None,
        BanRecord::Fresh,
        Some("bot-attempt-same"),
    )
    .await
    .unwrap();
    assert_eq!(filled, Some(row_id));
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].active);
    assert_eq!(rows[0].id, row_id);
    assert_eq!(rows[0].moderator_id, Some(9));
    assert_eq!(rows[0].moderator_name.as_deref(), Some("bot"));
    assert_eq!(rows[0].reason.as_deref(), Some("spam"));
    assert_eq!(rows[0].user_name.as_deref(), Some("member"));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), None, 50)
                .await
                .unwrap()
        ),
        0
    );
}

#[tokio::test]
async fn stale_attributed_manual_mirror_records_a_distinct_gateway_ban() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some("2024-01-01T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: old_id } = first else {
        panic!("first manual ban must insert, got {first:?}");
    };
    attribute_enrolled_ban(
        &f.db,
        Some(scope.clone()),
        old_id,
        55,
        77,
        Some("mod-one"),
        "first reason",
    )
    .await
    .unwrap();
    let replacement = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        Some("member"),
        None,
        Some("2024-06-01T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: new_id } = replacement else {
        panic!("a distinct manual ban must insert, got {replacement:?}");
    };
    assert_ne!(new_id, old_id);
    let rows = list_enrolled_bans(&f.db, scope.clone(), 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    assert_eq!(rows.iter().find(|row| row.active).unwrap().id, new_id);
    let ended = rows.iter().find(|row| row.id == old_id).unwrap();
    assert!(!ended.active);
    assert_eq!(ended.reason.as_deref(), Some("first reason"));
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), Some(55), 50)
                .await
                .unwrap()
        ),
        2
    );
    attribute_enrolled_ban(
        &f.db,
        Some(scope.clone()),
        new_id,
        55,
        88,
        Some("mod-two"),
        "second reason",
    )
    .await
    .unwrap();
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    let active = rows.iter().find(|row| row.active).unwrap();
    assert_eq!(active.moderator_id, Some(88));
    assert_eq!(active.reason.as_deref(), Some("second reason"));
}

#[tokio::test]
async fn observed_unban_after_placeholder_allows_a_new_manual_gateway_ban() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-02-01T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: old_id } = first else {
        panic!("first manual ban must insert, got {first:?}");
    };
    f.db.with_conn(|conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.execute_bounded(
            "INSERT INTO events (guild_id, event_type, user_id, data, created_at)
                 VALUES (?1, 'member_unban', ?2, '{}', ?3)",
            params![10_i64, 55_i64, "2024-05-01T00:00:00+00:00"],
            64 * 1024 * 1024,
        )?;
        tx.commit()?;
        Ok(())
    })
    .await
    .unwrap();
    let replacement = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-06-01T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: new_id } = replacement else {
        panic!("unban-healed mirror must allow a new ban, got {replacement:?}");
    };
    assert_ne!(new_id, old_id);
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), Some(55), 50)
                .await
                .unwrap()
        ),
        2
    );
}

#[tokio::test]
async fn bot_owned_echo_keeps_the_same_attempt_and_replaces_a_stale_one() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let stale = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-01-01T00:00:00+00:00"),
        true,
        Some("old-attempt"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id: stale_id } = stale else {
        panic!("stale echo must store a row, got {stale:?}");
    };
    let same = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-01-02T00:00:00+00:00"),
        true,
        Some("old-attempt"),
        true,
    )
    .await
    .unwrap();
    assert_eq!(same, GatewayBanWrite::Existing { row_id: stale_id });
    let fresh = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-06-01T00:00:00+00:00"),
        true,
        Some("new-attempt"),
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::OwnedByBot { row_id: new_id } = fresh else {
        panic!("a new bot attempt must store its own row, got {fresh:?}");
    };
    assert_ne!(new_id, stale_id);
    let rows = list_enrolled_bans(&f.db, scope, 10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|row| row.active).count(), 1);
    assert_eq!(rows.iter().find(|row| row.active).unwrap().id, new_id);
}

#[tokio::test]
async fn directory_absence_replaces_a_gateway_placeholder_without_an_unban_event() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-03-01T00:00:00+00:00"),
        false,
        None,
        true,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: old_id } = first else {
        panic!("first manual ban must insert, got {first:?}");
    };
    let replacement = record_gateway_ban(
        &f.db,
        Some(scope.clone()),
        55,
        None,
        None,
        Some("2024-04-01T00:00:00+00:00"),
        false,
        None,
        false,
    )
    .await
    .unwrap();
    let GatewayBanWrite::Recorded { row_id: new_id } = replacement else {
        panic!("directory transition must insert a new row, got {replacement:?}");
    };
    assert_ne!(new_id, old_id);
    assert_eq!(
        member_ban_events(
            &query_enrolled_events(&f.db, scope, Some("member_ban"), Some(55), 50)
                .await
                .unwrap()
        ),
        2
    );
}
