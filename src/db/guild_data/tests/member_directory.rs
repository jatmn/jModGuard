use super::*;

async fn ready() -> (Fixture, GuildDataStore, GuildDataScope) {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let placement = f.placement(&reg, 10).await;
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let scope = GuildDataScope::new(
        placement,
        crate::db::GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    store.bind_initial_membership(scope.clone()).await.unwrap();
    (f, store, scope)
}

#[tokio::test]
async fn version_eight_upgrade_admits_existing_enrollment_once() {
    use crate::db_compat::Connection;
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let placement = f.placement(&reg, 10).await;
    let raw = Connection::open(&f.store_path).unwrap();
    for schema in [
        SCHEMA,
        TENANT_SCHEMA,
        CASE_SCHEMA,
        HISTORY_SCHEMA,
        SOURCE_SCHEMA,
        EVIDENCE_SCHEMA,
        BUNNY_SCHEMA,
        ERASURE_SCHEMA,
    ] {
        raw.execute_batch(schema).unwrap();
    }
    raw.execute(
        "INSERT INTO store_identity VALUES(1,?1,?2,NULL)",
        params![FAMILY, reg.id().as_str()],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO guild_bindings VALUES(10,?1)",
        params![placement.revision()],
    )
    .unwrap();
    raw.execute_batch("INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')").unwrap();
    let legacy_payload = r#"{"source":{"discord_audit":{"entry_id":"99"}},"action":"ban","outcome":"succeeded","subject_id":"77","actor_id":null,"occurred_at_ms":900,"reason":"legacy","evidence":"unavailable"}"#;
    raw.execute("INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','cccccccccccccccccccccccccccccccc','discord:99',?1,0)",params![legacy_payload]).unwrap();
    raw.pragma_update(None, "user_version", 8).unwrap();
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    let scope = GuildDataScope::new(
        placement,
        crate::db::GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert_eq!(
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap()
            .status,
        "queued"
    );
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    assert!(
        store
            .get_member(scope.clone(), 77)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
    assert_eq!(
        store
            .member_history(scope.clone(), 77, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    store.db.with_conn(|conn|{let mut tx=conn.transaction_primary_read()?;tx.ensure_mutation_receipt()?;tx.execute("UPDATE guild_cases SET deleted=1,payload=NULL WHERE guild_id=10 AND case_id='cccccccccccccccccccccccccccccccc'",())?;tx.commit()}).await.unwrap();
    assert!(
        store
            .member_history(scope.clone(), 77, None, 50)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.execute("DELETE FROM guild_member_seed WHERE guild_id=10", ())?;
            tx.commit()
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .initialize_member_directory(scope, 42, 2_000)
            .await
            .unwrap()
            .status,
        "needs_attention"
    );
}

fn page(phase: MemberSyncPhase, cursor: Option<String>, terminal: bool) -> MemberSyncPage {
    MemberSyncPage {
        phase,
        expected_cursor: cursor,
        next_cursor: None,
        members: vec![],
        bans: vec![],
        audits: vec![],
        verification: None,
        terminal,
        total_estimate: Some(2),
        page_size: 100,
        pace_ms: 15_000,
        active_guild_count: 1,
    }
}

#[tokio::test]
async fn one_time_seed_batch_search_and_missing_checkpoint_fail_closed() {
    let (_f, store, scope) = ready().await;
    let initial = store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    assert_eq!(initial.status, "queued");
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut first = page(MemberSyncPhase::Members, None, false);
    first.members = vec![
        MemberObservation {
            user_id: 9_007_199_254_740_993,
            username: Some(Some("Alpha".into())),
            global_name: None,
            nickname: Some(Some("Émoji 🐇".into())),
            joined_at_ms: Some(500),
            observed_at_ms: 1_000,
            is_join: true,
        },
        MemberObservation {
            user_id: 9_007_199_254_740_994,
            username: Some(Some("Second".into())),
            global_name: None,
            nickname: None,
            joined_at_ms: None,
            observed_at_ms: 1_000,
            is_join: true,
        },
    ];
    first.next_cursor = Some("9007199254740994".into());
    let status = store
        .commit_member_page(scope.clone(), claim, first, 1_001)
        .await
        .unwrap();
    assert_eq!(status.processed, 2);
    let members = store
        .list_members(
            scope.clone(),
            MemberQuery {
                q: Some("éMOJI".into()),
                status: MemberStatusFilter::Current,
                joined_from_ms: None,
                joined_to_ms: None,
                sort: MemberSort::JoinedNewest,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(members.members.len(), 1);
    assert_eq!(members.members[0].user_id, "9007199254740993");
    let complete_members = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(complete_members.phase, MemberSyncPhase::Members);
    store
        .commit_member_page(
            scope.clone(),
            complete_members.clone(),
            page(
                MemberSyncPhase::Members,
                complete_members.cursor.clone(),
                true,
            ),
            16_002,
        )
        .await
        .unwrap();
    let bans = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 31_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut ban_page = page(MemberSyncPhase::Bans, bans.cursor.clone(), true);
    ban_page.bans.push(MemberBanSnapshot {
        user_id: 9_007_199_254_740_993,
        username: Some("Alpha".into()),
        global_name: None,
        reason: Some("known current reason".into()),
    });
    store
        .commit_member_page(scope.clone(), bans.clone(), ban_page, 31_003)
        .await
        .unwrap();
    let after_ban = store
        .list_members(
            scope.clone(),
            MemberQuery {
                q: Some("éMOJI".into()),
                status: MemberStatusFilter::Banned,
                joined_from_ms: None,
                joined_to_ms: None,
                sort: MemberSort::JoinedNewest,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        after_ban.members.len(),
        1,
        "ban snapshot must preserve nickname search"
    );
    assert_eq!(
        after_ban.members[0].current_ban_reason.as_deref(),
        Some("known current reason")
    );
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    let audit = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 46_003, 90_000)
        .await
        .unwrap()
        .unwrap();
    let ban_audit = store
        .commit_member_page(
            scope.clone(),
            audit.clone(),
            page(MemberSyncPhase::Audit, audit.cursor.clone(), true),
            46_004,
        )
        .await
        .unwrap();
    assert_eq!(ban_audit.phase.as_deref(), Some("audit"));
    let unban_claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 61_004, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unban_claim.cursor.as_deref(), Some("unban:"));
    let completed = store
        .commit_member_page(
            scope.clone(),
            unban_claim.clone(),
            page(MemberSyncPhase::Audit, unban_claim.cursor, true),
            61_005,
        )
        .await
        .unwrap();
    assert_eq!(completed.initial_completed_at, Some(61_005));
    assert_eq!(completed.status, "complete");
    assert!(
        store
            .claim_member_sync(scope.clone(), 42, "worker".into(), 100_000, 90_000)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .initialize_member_directory(scope.clone(), 42, 200_000)
            .await
            .unwrap()
            .initial_completed_at,
        Some(61_005)
    );
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.execute("DELETE FROM guild_member_seed WHERE guild_id=10", ())?;
            tx.commit()
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .initialize_member_directory(scope, 42, 300_000)
            .await
            .unwrap()
            .status,
        "needs_attention"
    );
}

#[tokio::test]
async fn ban_retains_stay_and_same_state_delivery_remains_unresolved_observation() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: 77,
                username: Some(Some("join".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(500),
                observed_at_ms: 1_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    assert!(store.observe_ban(scope.clone(), 77, 2_000).await.unwrap());
    assert!(store.observe_ban(scope.clone(), 77, 2_001).await.unwrap());
    let departed = store
        .depart_member(scope.clone(), 77, 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(departed.completed_stay_ms, Some(2_500));
    assert!(store.observe_unban(scope.clone(), 77, 4_000).await.unwrap());
    assert!(store.observe_ban(scope.clone(), 77, 5_000).await.unwrap());
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 10_000_000)
            .await
            .unwrap()
            .is_none()
    );
    let profile = store.get_member(scope, 77).await.unwrap().unwrap();
    assert!(profile.ever_banned);
}

#[tokio::test]
async fn pending_ban_fences_cleanup_until_verified_or_failed() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute(
                "UPDATE guild_member_seed SET initial_completed_at_ms=2000 WHERE guild_id=10",
                (),
            )?;
            tx.execute(
                "UPDATE guild_member_sync SET phase='complete',status='complete' WHERE guild_id=10",
                (),
            )?;
            tx.commit()
        })
        .await
        .unwrap();
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: 91,
                username: None,
                global_name: None,
                nickname: None,
                joined_at_ms: Some(500),
                observed_at_ms: 1_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    store.depart_member(scope.clone(), 91, 3_000).await.unwrap();
    let candidate = store
        .next_member_cleanup_candidate(scope.clone(), 400_000)
        .await
        .unwrap()
        .unwrap();
    let operation = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    store
        .begin_member_ban_attempt(scope.clone(), 91, operation.clone(), 400_001)
        .await
        .unwrap();
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_002)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .settle_member_cleanup(scope.clone(), candidate, false, 400_002)
            .await
            .is_err()
    );
    store
        .finish_member_ban_attempt(
            scope.clone(),
            91,
            &operation,
            CaseOutcome::Uncertain,
            400_003,
        )
        .await
        .unwrap();
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_004)
            .await
            .unwrap()
            .is_none()
    );
    let pending = store
        .next_member_pending_ban_verification(scope.clone(), 700_001)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_pending_ban_verification(scope.clone(), pending, false, 700_002)
        .await
        .unwrap();
    let candidate = store
        .next_member_cleanup_candidate(scope.clone(), 1_100_002)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_cleanup(scope.clone(), candidate, false, 1_100_002)
        .await
        .unwrap();
    assert!(store.get_member(scope.clone(), 91).await.unwrap().is_none());
    store
        .begin_member_ban_attempt(scope.clone(), 92, operation.clone(), 2_000_000)
        .await
        .unwrap();
    store
        .finish_member_ban_attempt(
            scope.clone(),
            92,
            &operation,
            CaseOutcome::Succeeded,
            2_000_001,
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope, 92)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
}

fn observed(user_id: i64, joined_at_ms: Option<i64>, observed_at_ms: i64) -> MemberObservation {
    MemberObservation {
        user_id,
        username: None,
        global_name: None,
        nickname: None,
        joined_at_ms,
        observed_at_ms,
        is_join: true,
    }
}

async fn complete_for_checks(store: &GuildDataStore, scope: &GuildDataScope) {
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute(
                "UPDATE guild_member_seed SET initial_completed_at_ms=2000 WHERE guild_id=10",
                (),
            )?;
            tx.execute(
                "UPDATE guild_member_sync SET phase='complete',status='complete' WHERE guild_id=10",
                (),
            )?;
            tx.commit()
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn live_and_page_rejoins_use_new_stays_without_old_join_reuse() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(10, Some(500), 1_000))
        .await
        .unwrap();
    store.depart_member(scope.clone(), 10, 2_000).await.unwrap();
    store
        .observe_member(scope.clone(), observed(10, Some(2_500), 3_000))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_member(scope.clone(), 10)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ms,
        Some(2_500)
    );
    let stays: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM guild_member_stays WHERE guild_id=10 AND user_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        stays, 1,
        "ordinary prior stay must be purged on live rejoin"
    );
    assert_eq!(
        store
            .depart_member(scope.clone(), 10, 4_000)
            .await
            .unwrap()
            .unwrap()
            .completed_stay_ms,
        Some(1_500)
    );
    store
        .observe_member(scope.clone(), observed(10, None, 5_000))
        .await
        .unwrap();
    assert_eq!(
        store
            .get_member(scope.clone(), 10)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ms,
        None,
        "unknown new join must not inherit old stay date"
    );

    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 5_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut p = page(MemberSyncPhase::Members, None, false);
    p.members.push(observed(10, Some(5_500), 5_500));
    store
        .commit_member_page(scope.clone(), claim, p, 5_501)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_member(scope.clone(), 10)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ms,
        Some(5_500)
    );
    let stay: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT joined_at_ms FROM guild_member_stays WHERE guild_id=10 AND user_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(stay, 5_500);
}

#[tokio::test]
async fn changed_join_and_delayed_leave_preserve_new_stay() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(11, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 11, 1_100).await.unwrap();
    store
        .observe_member(scope.clone(), observed(11, Some(3_000), 3_100))
        .await
        .unwrap();
    let decision = store
        .depart_member_checked(scope.clone(), 11, 3_200, Some(500))
        .await
        .unwrap();
    assert!(matches!(
        decision,
        MemberDepartureDecision::NeedsVerification
    ));
    assert!(
        store
            .get_member(scope.clone(), 11)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
    let check = store
        .next_member_target_check(scope.clone(), 3_200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.kind, MemberVerifyKind::Member);
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 11,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present: true,
                member: Some(observed(11, Some(3_000), 3_300)),
                ban: None,
            },
            3_300,
        )
        .await
        .unwrap();
    let stays=store.db.with_conn(|conn|{let mut stmt=conn.prepare("SELECT joined_at_ms,ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=11 ORDER BY joined_at_ms")?;let rows=stmt.query_map((),|r|Ok((r.get::<i64>(0)?,r.get::<bool>(1)?)))?;rows.collect::<std::result::Result<Vec<_>,_>>()} ).await.unwrap();
    assert_eq!(stays, vec![(500, true), (3_000, false)]);
    assert!(matches!(
        store
            .depart_member_checked(scope.clone(), 11, 4_000, Some(3_000))
            .await
            .unwrap(),
        MemberDepartureDecision::Applied(Some(_))
    ));
}

#[tokio::test]
async fn missing_cache_departure_uses_saved_join_and_positive_update_starts_new_stay() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(21, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 21, 1_100).await.unwrap();

    let decision = store
        .depart_member_checked(scope.clone(), 21, 2_000, None)
        .await
        .unwrap();
    let MemberDepartureDecision::Applied(Some(departure)) = decision else {
        panic!("saved current stay should be applied without a Gateway cache token");
    };
    assert_eq!(departure.joined_at_ms, Some(500));
    assert_eq!(departure.completed_stay_ms, Some(1_500));

    let mut update = observed(21, Some(2_500), 3_000);
    update.is_join = false;
    store.observe_member(scope.clone(), update).await.unwrap();
    let member = store.get_member(scope.clone(), 21).await.unwrap().unwrap();
    assert!(member.is_current);
    assert_eq!(member.joined_at_ms, Some(2_500));
    assert_eq!(member.departed_at_ms, None);
    let stays = store.db.with_conn(|conn| {
        let mut stmt = conn.prepare("SELECT joined_at_ms,departed_at_ms FROM guild_member_stays WHERE guild_id=10 AND user_id=21 ORDER BY joined_at_ms")?;
        let rows = stmt.query_map((), |r| Ok((r.get::<Option<i64>>(0)?, r.get::<Option<i64>>(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>,_>>()
    }).await.unwrap();
    assert_eq!(stays, vec![(Some(500), Some(2_000)), (Some(2_500), None)]);
}

#[tokio::test]
async fn absent_update_without_join_time_queues_bounded_member_check() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(22, Some(500), 1_000))
        .await
        .unwrap();
    store.depart_member(scope.clone(), 22, 2_000).await.unwrap();
    let mut update = observed(22, None, 3_000);
    update.is_join = false;
    store.observe_member(scope.clone(), update).await.unwrap();
    assert!(
        !store
            .get_member(scope.clone(), 22)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
    let check = store
        .next_member_target_check(scope.clone(), 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 22);
    assert_eq!(check.kind, MemberVerifyKind::Member);
}

#[tokio::test]
async fn delayed_old_join_update_cannot_resurrect_departed_member() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(23, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 23, 1_100).await.unwrap();
    store.depart_member(scope.clone(), 23, 2_000).await.unwrap();
    let mut delayed = observed(23, Some(500), 3_000);
    delayed.is_join = false;
    delayed.username = Some(Some("new display".into()));
    store.observe_member(scope.clone(), delayed).await.unwrap();
    let member = store.get_member(scope.clone(), 23).await.unwrap().unwrap();
    assert!(!member.is_current);
    assert_eq!(member.joined_at_ms, Some(500));
    assert_eq!(member.departed_at_ms, Some(2_000));
    let check = store
        .next_member_target_check(scope.clone(), 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 23);
    assert_eq!(check.revision, member.revision);
    let stays: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM guild_member_stays WHERE guild_id=10 AND user_id=23",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(stays, 1);
}

#[tokio::test]
async fn older_join_token_cannot_replace_current_newer_stay() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(24, Some(3_000), 3_100))
        .await
        .unwrap();
    let mut delayed = observed(24, Some(500), 3_200);
    delayed.is_join = false;
    delayed.username = Some(Some("updated display".into()));
    store.observe_member(scope.clone(), delayed).await.unwrap();
    let member = store.get_member(scope.clone(), 24).await.unwrap().unwrap();
    assert!(member.is_current);
    assert_eq!(member.joined_at_ms, Some(3_000));
    let check = store
        .next_member_target_check(scope.clone(), 3_200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 24);
    assert_eq!(check.revision, member.revision);
    let stays: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM guild_member_stays WHERE guild_id=10 AND user_id=24",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(stays, 1);
}

#[tokio::test]
async fn member_total_is_exact_after_terminal_page_and_survives_later_stages() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut first = page(MemberSyncPhase::Members, None, false);
    first.members = vec![
        observed(31, Some(500), 1_000),
        observed(32, Some(500), 1_000),
    ];
    first.total_estimate = Some(999);
    first.next_cursor = Some("32".into());
    let queued = store
        .commit_member_page(scope.clone(), claim, first, 1_001)
        .await
        .unwrap();
    assert_eq!(queued.total_estimate, Some(999));
    assert!(queued.total_is_estimated);

    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut terminal = page(MemberSyncPhase::Members, claim.cursor.clone(), true);
    terminal.members = vec![observed(33, Some(500), 16_001)];
    terminal.total_estimate = Some(999);
    let after_members = store
        .commit_member_page(scope.clone(), claim, terminal, 16_002)
        .await
        .unwrap();
    assert_eq!(after_members.phase.as_deref(), Some("bans"));
    assert_eq!(after_members.total_estimate, Some(3));
    assert!(!after_members.total_is_estimated);

    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 31_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, claim.cursor.clone(), true);
    bans.total_estimate = Some(1_000);
    let after_bans = store
        .commit_member_page(scope.clone(), claim, bans, 31_003)
        .await
        .unwrap();
    assert_eq!(after_bans.total_estimate, Some(3));
    assert!(!after_bans.total_is_estimated);
}

#[tokio::test]
async fn queued_eta_is_anchored_then_becomes_unknown_without_progress() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut first = page(MemberSyncPhase::Members, None, false);
    first.members = vec![observed(41, Some(500), 1_000)];
    first.next_cursor = Some("41".into());
    first.total_estimate = Some(100);
    first.page_size = 10;
    first.active_guild_count = 3;
    let first_status = store
        .commit_member_page(scope.clone(), claim, first, 1_001)
        .await
        .unwrap();
    assert_eq!(first_status.status, "queued");
    let finish = first_status.phase_finish_at.unwrap();
    let later = store
        .member_sync_status(scope.clone(), 42, 10_001)
        .await
        .unwrap();
    assert_eq!(later.phase_finish_at, Some(finish));
    assert!(later.phase_remaining_seconds.unwrap() < first_status.phase_remaining_seconds.unwrap());
    let stale = store
        .member_sync_status(scope.clone(), 42, 121_002)
        .await
        .unwrap();
    assert_eq!(stale.status, "queued");
    assert_eq!(stale.phase_finish_at, None);
    assert_eq!(stale.phase_remaining_seconds, None);
    assert_eq!(stale.estimate_reason.as_deref(), Some("worker_unconfirmed"));
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 121_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .defer_member_sync(
            scope.clone(),
            claim,
            "rate_limited".into(),
            200_000,
            121_003,
        )
        .await
        .unwrap();
    let waiting = store
        .member_sync_status(scope.clone(), 42, 121_003)
        .await
        .unwrap();
    assert_eq!(waiting.status, "waiting");
    assert_eq!(waiting.status_reason.as_deref(), Some("rate_limited"));
    assert_eq!(waiting.next_attempt_at, Some(200_000));
    assert_eq!(waiting.phase_finish_at, None);
    assert_eq!(
        waiting.estimate_reason.as_deref(),
        Some("waiting for retry; remaining work cannot be estimated")
    );
}

#[tokio::test]
async fn revision_stale_page_can_release_only_its_claim_at_normal_pace() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .observe_member(scope.clone(), observed(51, Some(500), 1_001))
        .await
        .unwrap();
    let stale = store
        .commit_member_page(
            scope.clone(),
            claim.clone(),
            page(MemberSyncPhase::Members, None, false),
            1_002,
        )
        .await;
    assert!(
        stale
            .unwrap_err()
            .downcast_ref::<MemberSyncError>()
            .is_some_and(|e| matches!(e, MemberSyncError::Stale))
    );
    store
        .defer_member_sync(
            scope.clone(),
            claim.clone(),
            "directory_changed".into(),
            16_002,
            1_002,
        )
        .await
        .unwrap();
    assert!(
        store
            .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
            .await
            .unwrap()
            .is_none()
    );
    let retry = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.cursor, claim.cursor);
    assert_eq!(retry.run_id, claim.run_id);
    assert!(retry.generation > claim.generation);
    assert!(
        store
            .defer_member_sync(scope.clone(), claim, "late".into(), 31_002, 16_003)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn verified_absence_has_unknown_event_time_and_gap_cleanup_is_targeted() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store
        .observe_member(scope.clone(), observed(12, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 12, 1_500).await.unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            12,
            MemberVerifyKind::Member,
            "missed_event",
            2_000,
        )
        .await
        .unwrap();
    let check = store
        .next_member_target_check(scope.clone(), 2_000)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 12,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present: false,
                member: None,
                ban: None,
            },
            2_100,
        )
        .await
        .unwrap();
    let history = store
        .member_history(scope.clone(), 12, None, 50)
        .await
        .unwrap();
    let missing = history
        .entries
        .iter()
        .find(|e| e.kind == "member_state_observation")
        .unwrap();
    assert_eq!(missing.occurred_at_ms, None);
    assert_eq!(missing.observed_at_ms, Some(2_100));
    assert!(missing.case_id.is_none());
    let first = store
        .member_history(scope.clone(), 12, None, 1)
        .await
        .unwrap();
    assert_eq!(first.entries[0].kind, "member_state_observation");
    let second = store
        .member_history(scope.clone(), 12, first.next_cursor, 1)
        .await
        .unwrap();
    assert_eq!(
        second.entries[0].kind, "join",
        "unknown event time paginates by verification time without inventing a dated departure"
    );
    let ended: bool = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=12",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert!(ended);
    store.observe_ban(scope.clone(), 13, 2_200).await.unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            13,
            MemberVerifyKind::Ban,
            "missed_event",
            2_300,
        )
        .await
        .unwrap();
    let ban_check = store
        .next_member_target_check(scope.clone(), 2_300)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_target_check(
            scope.clone(),
            ban_check.clone(),
            MemberVerification {
                user_id: 13,
                revision: ban_check.revision,
                kind: MemberVerifyKind::Ban,
                present: false,
                member: None,
                ban: None,
            },
            2_400,
        )
        .await
        .unwrap();
    let ban_history = store
        .member_history(scope.clone(), 13, None, 50)
        .await
        .unwrap();
    assert!(
        ban_history
            .entries
            .iter()
            .any(|e| e.kind == "ban_state_observation"
                && e.occurred_at_ms.is_none()
                && e.observed_at_ms == Some(2_400))
    );
    assert!(
        !store
            .get_member(scope.clone(), 13)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );

    store
        .observe_member(scope.clone(), observed(14, Some(500), 2_500))
        .await
        .unwrap();
    store.depart_member(scope.clone(), 14, 3_000).await.unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 3_100)
        .await
        .unwrap();
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_000)
            .await
            .unwrap()
            .is_none()
    );
    let check = store
        .next_member_target_check(scope.clone(), 400_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 14);
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 14,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present: false,
                member: None,
                ban: None,
            },
            400_001,
        )
        .await
        .unwrap();
    let candidate = store
        .next_member_cleanup_candidate(scope.clone(), 400_001)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_cleanup(scope.clone(), candidate, false, 400_002)
        .await
        .unwrap();
    assert!(store.get_member(scope.clone(), 14).await.unwrap().is_none());
}

#[tokio::test]
async fn retry_clears_only_permission_deadline_and_audit_only_keeps_completion() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .defer_member_sync(
            scope.clone(),
            claim,
            "permission_denied".into(),
            3_601_000,
            1_001,
        )
        .await
        .unwrap();
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "retry-once".into(),
            2_000,
        )
        .await
        .unwrap();
    assert!(
        store
            .claim_member_sync(scope.clone(), 42, "worker".into(), 2_000, 90_000)
            .await
            .unwrap()
            .is_some()
    );
    store.db.with_conn(|conn|{let mut tx=conn.transaction_primary_read()?;tx.ensure_mutation_receipt()?;tx.execute("UPDATE guild_member_seed SET initial_completed_at_ms=5000 WHERE guild_id=10",())?;tx.execute("UPDATE guild_member_sync SET phase='complete',status='complete',lease_owner=NULL,lease_until_ms=NULL WHERE guild_id=10",())?;tx.commit()}).await.unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 6_000)
        .await
        .unwrap();
    let status = store
        .member_sync_status(scope.clone(), 42, 6_000)
        .await
        .unwrap();
    assert_eq!(status.initial_completed_at, Some(5_000));
    assert!(status.audit_recovery_pending);
    let ban = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 6_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ban.cursor, None);
    store
        .commit_member_audit_recovery_page(scope.clone(), ban, vec![], (None, true), 6_001, 15_000)
        .await
        .unwrap();
    let unban = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 21_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unban.cursor.as_deref(), Some("unban:"));
    store
        .mark_member_reconciliation_needed(scope.clone(), "another_gateway_gap", 21_001)
        .await
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            unban,
            vec![],
            (None, true),
            21_002,
            15_000,
        )
        .await
        .unwrap();
    let followup = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 36_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        followup.cursor, None,
        "gap after first cursor must restart at newest audit entries"
    );
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            followup,
            vec![],
            (None, true),
            36_003,
            15_000,
        )
        .await
        .unwrap();
    let followup_unban = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 51_003, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            followup_unban,
            vec![],
            (None, true),
            51_004,
            15_000,
        )
        .await
        .unwrap();
    let done = store
        .member_sync_status(scope.clone(), 42, 51_004)
        .await
        .unwrap();
    assert_eq!(done.initial_completed_at, Some(5_000));
    assert_eq!(done.audit_recovery_status.as_deref(), Some("complete"));
    assert_eq!(done.status, "complete");
}

#[tokio::test]
async fn previously_banned_excludes_current_and_pending_success_respects_newer_unban() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store.observe_ban(scope.clone(), 20, 2_000).await.unwrap();
    store.observe_ban(scope.clone(), 21, 2_000).await.unwrap();
    store.observe_unban(scope.clone(), 21, 3_000).await.unwrap();
    let previous = store
        .list_members(
            scope.clone(),
            MemberQuery {
                q: None,
                status: MemberStatusFilter::PreviouslyBanned,
                joined_from_ms: None,
                joined_to_ms: None,
                sort: MemberSort::JoinedNewest,
                after: None,
                limit: 50,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        previous
            .members
            .iter()
            .map(|m| m.user_id.as_str())
            .collect::<Vec<_>>(),
        vec!["21"]
    );
    let op = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    store
        .begin_member_ban_attempt(scope.clone(), 21, op.clone(), 4_000)
        .await
        .unwrap();
    assert!(
        store.observe_unban(scope.clone(), 21, 4_100).await.unwrap(),
        "same-state delivery is an unresolved observation"
    );
    store
        .finish_member_ban_attempt(scope.clone(), 21, &op, CaseOutcome::Succeeded, 4_200)
        .await
        .unwrap();
    assert!(
        !store
            .get_member(scope.clone(), 21)
            .await
            .unwrap()
            .unwrap()
            .is_banned,
        "later observed unban wins current-state fence"
    );
    let check = store
        .next_member_target_check(scope.clone(), 4_200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.kind, MemberVerifyKind::Ban);
}
