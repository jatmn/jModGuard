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
            tx.execute("INSERT INTO events(guild_id,event_type,user_id,data,created_at) VALUES(10,'member_ban',?1,'{\"snowflake\":1329331947876479006}','now')", params![i])?;
            tx.execute("INSERT INTO bans(guild_id,user_id,reason,created_at,active) VALUES(10,?1,'fixture','now',?2)", params![i, i % 2])?;
        }
        tx.commit()
    }).await.unwrap();
    insert_event(
        &f.db,
        20,
        "member_ban",
        Some(204),
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
    let rows = query_enrolled_events(&f.db, scope.clone(), Some("member_ban"), Some(5), 50)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, Some(5));
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
async fn moderation_reads_create_no_receipts_or_version_changes_and_fail_closed() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let version = get_config_version(&f.db).await.unwrap();
    f.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_read_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'read wrote receipt'); END")).await.unwrap();
    query_enrolled_events(&f.db, scope.clone(), None, None, 50)
        .await
        .unwrap();
    list_enrolled_bans(&f.db, scope.clone(), 100).await.unwrap();
    enrolled_guild_stats(&f.db, scope.clone()).await.unwrap();
    assert_eq!(get_config_version(&f.db).await.unwrap(), version);
    f.db.with_conn(|conn| {
        conn.execute_batch("ALTER TABLE guild_enrollments RENAME TO unavailable_enrollments")
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
async fn moderation_read_rechecks_membership_after_waiting_for_primary_writer() {
    let f = Fixture::new();
    let scope = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let writer_db = f.db.clone();
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let writer = tokio::spawn(async move {
        writer_db.with_conn(move |conn| {
            let tx = conn.transaction_immediate()?;
            tx.execute("UPDATE guild_enrollments SET id='replacement',joined_at_ns=101 WHERE guild_id=10", params![])?;
            locked.send(()).unwrap();
            wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            tx.commit()
        }).await.unwrap();
    });
    ready.await.unwrap();
    let read = enrolled_guild_stats(&f.db, scope);
    tokio::pin!(read);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut read)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    writer.await.unwrap();
    assert!(read.await.unwrap_err().is::<EnrollmentChanged>());
}
