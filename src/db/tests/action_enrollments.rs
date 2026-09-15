use super::*;
use crate::bot_actions::{Action, Limits};

struct Fixture {
    db: Db,
    path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let path = tmp_db_path("guild_enrollment");
        Self {
            db: Db::open(&path, 2).unwrap(),
            path,
        }
    }
    async fn enqueue(&self, key: &str, action: Action) -> BotAction {
        enqueue_bot_action(
            &self.db,
            10,
            42,
            &format!("1000_{key}"),
            action,
            1000,
            Limits::default(),
        )
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup_db(&self.path);
    }
}
fn unban() -> Action {
    Action::Unban {
        user_id: 99,
        reason: "appeal".into(),
    }
}

#[tokio::test]
async fn unregistered_and_retired_guilds_cannot_admit_or_observe_queue_work() {
    let f = Fixture::new();
    placed_before_erasure(&f.db, 10).await;
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_before",
            unban(),
            1000,
            Limits::default()
        )
        .await
        .is_err()
    );
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let job = f.enqueue("old", unban()).await;
    let claim = claim_bot_action(&f.db, 1000).await.unwrap().unwrap();
    retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
    assert!(!begin_bot_action(&f.db, &claim, 1000).await.unwrap());
    assert!(bot_action(&f.db, 10, &job.id).await.unwrap().is_none());
    assert!(recent_bot_actions(&f.db, 10).await.unwrap().is_empty());
    assert!(!cancel_bot_action(&f.db, 10, &job.id).await.unwrap());
    assert!(claim_bot_action(&f.db, 1000).await.unwrap().is_none());
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_after",
            unban(),
            1000,
            Limits::default()
        )
        .await
        .is_err()
    );
    enroll_guild(&f.db, 10, 101).await.unwrap();
    assert!(
        enqueue_bot_action(&f.db, 10, 42, "1000_old", unban(), 1000, Limits::default())
            .await
            .is_err()
    );
    assert!(bot_action(&f.db, 10, &job.id).await.unwrap().is_none());
    let new = f.enqueue("new", unban()).await;
    assert_ne!(new.enrollment_id, job.enrollment_id);
}

#[tokio::test]
async fn old_claim_and_completion_cannot_change_new_enrollment_or_another_guild() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    enroll_guild(&f.db, 20, 100).await.unwrap();
    let job = f
        .enqueue(
            "old",
            Action::SetLogChannel {
                event: "message_edit".into(),
                channel_id: Some(55),
            },
        )
        .await;
    let claim = claim_bot_action(&f.db, 1000).await.unwrap().unwrap();
    assert_eq!(claim.id, job.id);
    assert!(begin_bot_action(&f.db, &claim, 1000).await.unwrap());
    let other = enqueue_bot_action(
        &f.db,
        20,
        42,
        "1000_other",
        unban(),
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    enroll_guild(&f.db, 10, 101).await.unwrap();
    assert_eq!(
        bot_action_dispatch(&f.db, &claim).await.unwrap(),
        BotActionDispatch::EnrollmentEnded
    );
    assert!(
        !finish_bot_action(&f.db, &claim, "succeeded", "log_channel_set")
            .await
            .unwrap()
    );
    f.db.with_conn(|conn| {
        assert_eq!(
            conn.query_row("SELECT count(*) FROM log_channels", (), |r| Ok(
                r.get::<i64>(0)?
            ))?,
            0
        );
        assert_eq!(
            conn.query_row("SELECT state FROM bot_actions WHERE guild_id=10", (), |r| {
                Ok(r.get::<String>(0)?)
            })?,
            "uncertain"
        );
        Ok(())
    })
    .await
    .unwrap();
    assert!(bot_action(&f.db, 20, &other.id).await.unwrap().is_some());
    // New membership is not blocked by the former membership's uncertain target.
    f.enqueue(
        "new",
        Action::SetLogChannel {
            event: "message_edit".into(),
            channel_id: Some(56),
        },
    )
    .await;
}

#[tokio::test]
async fn altered_claim_identity_is_rejected_before_dispatch_and_local_effects() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    f.enqueue("claim", unban()).await;
    let claim = claim_bot_action(&f.db, 1000).await.unwrap().unwrap();
    let mut forged = claim.clone();
    forged.guild_id = 20;
    assert!(!begin_bot_action(&f.db, &forged, 1000).await.unwrap());
    assert!(begin_bot_action(&f.db, &claim, 1000).await.unwrap());
    for field in 0..5 {
        let mut forged = claim.clone();
        match field {
            0 => forged.guild_id = 20,
            1 => forged.actor_id = 43,
            2 => {
                forged.action = Action::Unban {
                    user_id: 100,
                    reason: "forged".into(),
                }
            }
            3 => forged.enrollment_id = Some("forged".into()),
            _ => forged.joined_at_ns = Some(101),
        }
        assert_eq!(
            bot_action_dispatch(&f.db, &forged).await.unwrap(),
            BotActionDispatch::EnrollmentEnded
        );
        assert!(
            !finish_bot_action(&f.db, &forged, "succeeded", "unbanned")
                .await
                .unwrap()
        );
    }
    assert!(
        finish_bot_action(&f.db, &claim, "succeeded", "unbanned")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn enrollment_transition_and_queue_cancellation_roll_back_together() {
    let f = Fixture::new();
    let first = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let job = f.enqueue("rollback", unban()).await;
    f.db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_enrollment BEFORE UPDATE ON guild_enrollments
        BEGIN SELECT RAISE(ABORT,'test failure'); END",
        )
    })
    .await
    .unwrap();
    assert!(retire_guild_enrollment(&f.db, 10, 100).await.is_err());
    assert_eq!(guild_enrollment(&f.db, 10).await.unwrap(), Some(first));
    assert_eq!(
        bot_action(&f.db, 10, &job.id).await.unwrap().unwrap().state,
        "pending"
    );
}

#[tokio::test]
async fn unbound_earlier_queue_rows_are_never_dispatched_or_adopted() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let old = f.enqueue("unbound", unban()).await;
    f.db.with_conn(|conn| {
        conn.execute(
            "UPDATE bot_actions SET enrollment_id=NULL,joined_at_ns=NULL",
            (),
        )?;
        Ok(())
    })
    .await
    .unwrap();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    assert!(claim_bot_action(&f.db, 1000).await.unwrap().is_none());
    assert!(bot_action(&f.db, 10, &old.id).await.unwrap().is_none());
    assert!(
        enqueue_bot_action(
            &f.db,
            10,
            42,
            "1000_unbound",
            unban(),
            1000,
            Limits::default()
        )
        .await
        .is_err()
    );
    let new = f.enqueue("bound", unban()).await;
    assert_eq!(
        claim_bot_action(&f.db, 1000).await.unwrap().unwrap().id,
        new.id
    );
}

#[tokio::test]
async fn captured_admission_never_rebinds_new_or_retried_intents_after_membership_change() {
    for action in [
        unban(),
        Action::SetLogChannel {
            event: "message_edit".into(),
            channel_id: Some(55),
        },
    ] {
        for rejoin in [false, true] {
            let f = Fixture::new();
            placed_before_erasure(&f.db, 10).await;
            let old = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
            let job = enqueue_enrolled_bot_action(
                &f.db,
                &old,
                42,
                "1000_original",
                action.clone(),
                1000,
                Limits::default(),
            )
            .await
            .unwrap();
            retire_guild_enrollment(&f.db, 10, 100).await.unwrap();
            if rejoin {
                enroll_guild(&f.db, 10, 101).await.unwrap();
            }
            for request in ["1000_original", "1000_fresh_but_stale_scope"] {
                let error = enqueue_enrolled_bot_action(
                    &f.db,
                    &old,
                    42,
                    request,
                    action.clone(),
                    1001,
                    Limits::default(),
                )
                .await
                .unwrap_err();
                assert!(error.is::<EnrollmentChanged>(), "{error:#}");
            }
            f.db.with_conn(|conn| {
                assert_eq!(
                    conn.query_row("SELECT count(*) FROM bot_actions", (), |r| Ok(
                        r.get::<i64>(0)?
                    ))?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
            if rejoin {
                let current = guild_enrollment(&f.db, 10).await.unwrap().unwrap();
                assert!(
                    enqueue_enrolled_bot_action(
                        &f.db,
                        &current,
                        42,
                        "1000_original",
                        action.clone(),
                        1001,
                        Limits::default()
                    )
                    .await
                    .unwrap_err()
                    .is::<crate::bot_actions::RequestKeyError>()
                );
                let new = enqueue_enrolled_bot_action(
                    &f.db,
                    &current,
                    42,
                    "1000_explicit_new",
                    action.clone(),
                    1001,
                    Limits::default(),
                )
                .await
                .unwrap();
                assert_ne!(job.enrollment_id, new.enrollment_id);
            }
        }
    }
}

#[tokio::test]
async fn captured_queue_reads_and_cancel_refuse_old_membership_even_for_new_job_ids() {
    let f = Fixture::new();
    let old = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    enroll_guild(&f.db, 10, 101).await.unwrap();
    let current = guild_enrollment(&f.db, 10).await.unwrap().unwrap();
    let job = enqueue_enrolled_bot_action(
        &f.db,
        &current,
        42,
        "1000_current",
        unban(),
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    assert!(
        recent_enrolled_bot_actions(&f.db, &old)
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
    );
    for id in [&job.id, "missing"] {
        assert!(
            enrolled_bot_action(&f.db, &old, id)
                .await
                .unwrap_err()
                .is::<EnrollmentChanged>()
        );
        assert!(
            cancel_enrolled_bot_action(&f.db, &old, id)
                .await
                .unwrap_err()
                .is::<EnrollmentChanged>()
        );
    }
    assert_eq!(
        enrolled_bot_action(&f.db, &current, &job.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "pending"
    );
    assert_eq!(
        recent_enrolled_bot_actions(&f.db, &current)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        cancel_enrolled_bot_action(&f.db, &current, &job.id)
            .await
            .unwrap()
    );
    assert!(
        !cancel_enrolled_bot_action(&f.db, &current, &job.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn altered_admission_identity_and_membership_read_errors_leave_no_work() {
    let f = Fixture::new();
    let original = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    enroll_guild(&f.db, 20, 100).await.unwrap();
    for field in 0..3 {
        let mut altered = original.clone();
        match field {
            0 => altered.guild_id = 20,
            1 => altered.id = "forged".into(),
            _ => altered.joined_at_ns += 1,
        };
        assert!(
            enqueue_enrolled_bot_action(
                &f.db,
                &altered,
                42,
                "1000_altered",
                unban(),
                1000,
                Limits::default()
            )
            .await
            .unwrap_err()
            .is::<EnrollmentChanged>()
        );
    }
    f.db.with_conn(|conn| {
        conn.execute_batch("ALTER TABLE guild_enrollments RENAME TO fixture_unavailable")
    })
    .await
    .unwrap();
    assert!(
        enqueue_enrolled_bot_action(
            &f.db,
            &original,
            42,
            "1000_error",
            unban(),
            1000,
            Limits::default()
        )
        .await
        .unwrap_err()
        .is::<EnrollmentUnverified>()
    );
    assert!(
        recent_enrolled_bot_actions(&f.db, &original)
            .await
            .unwrap_err()
            .is::<EnrollmentUnverified>()
    );
    assert!(
        cancel_enrolled_bot_action(&f.db, &original, "missing")
            .await
            .unwrap_err()
            .is::<EnrollmentUnverified>()
    );
    f.db.with_conn(|conn| {
        assert_eq!(
            conn.query_row("SELECT count(*) FROM bot_actions", (), |r| Ok(
                r.get::<i64>(0)?
            ))?,
            0
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn captured_admission_preserves_deduplication_and_transaction_rollback() {
    let f = Fixture::new();
    let current = enroll_guild(&f.db, 10, 100).await.unwrap().unwrap();
    let first = enqueue_enrolled_bot_action(
        &f.db,
        &current,
        42,
        "1000_retry",
        unban(),
        1000,
        Limits::default(),
    )
    .await
    .unwrap();
    let repeated = enqueue_enrolled_bot_action(
        &f.db,
        &current,
        42,
        "1000_retry",
        unban(),
        1001,
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.id, repeated.id);
    f.db.with_conn(|conn|conn.execute_batch("CREATE TRIGGER reject_action AFTER INSERT ON bot_actions BEGIN SELECT RAISE(ABORT,'fixture failure'); END")).await.unwrap();
    assert!(
        enqueue_enrolled_bot_action(
            &f.db,
            &current,
            42,
            "1000_rollback",
            Action::SetLogChannel {
                event: "message_edit".into(),
                channel_id: Some(55)
            },
            1001,
            Limits::default()
        )
        .await
        .is_err()
    );
    assert_eq!(
        recent_enrolled_bot_actions(&f.db, &current)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn newer_log_writes_preserve_uncertain_work_from_a_former_membership() {
    let f = Fixture::new();
    enroll_guild(&f.db, 10, 100).await.unwrap();
    let action = Action::SetLogChannel {
        event: "member_join".into(),
        channel_id: Some(55),
    };
    let old = f.enqueue("old", action.clone()).await;
    let claim = claim_bot_action(&f.db, 1000).await.unwrap().unwrap();
    assert!(begin_bot_action(&f.db, &claim, 1000).await.unwrap());
    enroll_guild(&f.db, 10, 101).await.unwrap();
    let new = f.enqueue("new", action).await;
    upsert_log_channel(&f.db, 10, "member_join", 99)
        .await
        .unwrap();
    assert_eq!(
        bot_action(&f.db, 10, &new.id).await.unwrap().unwrap().state,
        "cancelled"
    );
    assert!(
        !finish_bot_action(&f.db, &claim, "succeeded", "late")
            .await
            .unwrap()
    );
    prune_bot_action_history(&f.db, i64::MAX / 2, Limits::default())
        .await
        .unwrap();
    f.db.with_conn(move |conn| {
        let (state, outcome): (String, String) = conn.query_row(
            "SELECT state,outcome FROM bot_actions WHERE id=?1",
            params![old.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(state, "uncertain");
        assert_eq!(outcome, "enrollment_ended");
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(list_log_channels(&f.db, 10).await.unwrap()[0].1, 99);
}

#[test]
fn version_seven_actions_are_not_adopted_by_an_existing_membership() {
    let path = tmp_db_path("action_enrollment_upgrade");
    let conn = Connection::open(&path).unwrap();
    for (index, sql) in MIGRATIONS[..7].iter().enumerate() {
        let tx = conn.transaction_primary_read().unwrap();
        tx.execute_batch(sql).unwrap();
        tx.pragma_update(None, "user_version", (index + 1) as i64)
            .unwrap();
        tx.commit().unwrap();
    }
    conn.execute_batch("INSERT INTO guild_enrollments VALUES(10,'existing-membership',100,1)")
        .unwrap();
    for (i, state) in ["pending", "claimed", "running", "uncertain", "succeeded"]
        .into_iter()
        .enumerate()
    {
        let action = Action::Unban {
            user_id: 100 + i as i64,
            reason: "before upgrade".into(),
        };
        conn.execute("INSERT INTO bot_actions(id,guild_id,actor_id,request_key,resource,payload,state,created_at,expires_at,request_created_at,claim,finished_at)
            VALUES(?1,10,42,?2,?3,?4,?5,1000,1300,1000,'old-claim',1000)",params![state,format!("1000_{state}"),action.resource(),serde_json::to_string(&action).unwrap(),state]).unwrap();
    }
    drop(conn);
    let db = Db::open(&path, 2).unwrap();
    db.with_conn_sync(|conn| {
        for (id, expected) in [
            ("pending", "cancelled"),
            ("claimed", "cancelled"),
            ("running", "uncertain"),
            ("uncertain", "uncertain"),
            ("succeeded", "succeeded"),
        ] {
            let (state, binding): (String, Option<String>) = conn.query_row(
                "SELECT state,enrollment_id FROM bot_actions WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(state, expected);
            assert!(binding.is_none());
        }
        Ok(())
    })
    .unwrap();
    assert!(
        tokio_test_block_on(claim_bot_action(&db, 1000))
            .unwrap()
            .is_none()
    );
    assert!(
        tokio_test_block_on(recent_bot_actions(&db, 10))
            .unwrap()
            .is_empty()
    );
    assert!(
        tokio_test_block_on(enqueue_bot_action(
            &db,
            10,
            42,
            "1000_pending",
            Action::Unban {
                user_id: 100,
                reason: "before upgrade".into()
            },
            1000,
            Limits::default()
        ))
        .unwrap_err()
        .is::<crate::bot_actions::RequestKeyError>()
    );
    let new = tokio_test_block_on(enqueue_bot_action(
        &db,
        10,
        42,
        "1000_new",
        unban(),
        1000,
        Limits::default(),
    ))
    .unwrap();
    assert_eq!(new.enrollment_id.as_deref(), Some("existing-membership"));
    drop(db);
    cleanup_db(&path);
}
