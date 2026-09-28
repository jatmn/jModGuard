use super::*;
use crate::db::write_budget::MemberSqlWrites;

const SCHEMA_BATCH_LIMIT: u64 = 64 * 1024 * 1024;

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
        raw.execute_batch_bounded(schema, SCHEMA_BATCH_LIMIT)
            .unwrap();
    }
    raw.execute_bounded(
        "INSERT INTO store_identity VALUES(1,?1,?2,NULL)",
        params![FAMILY, reg.id().as_str()],
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    raw.execute_bounded(
        "INSERT INTO guild_bindings VALUES(10,?1)",
        params![placement.revision()],
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    raw.execute_batch_bounded(
        "INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')",
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    let legacy_payload = r#"{"source":{"discord_audit":{"entry_id":"99"}},"action":"ban","outcome":"succeeded","subject_id":"77","actor_id":null,"occurred_at_ms":900,"reason":"legacy","evidence":"unavailable"}"#;
    raw.execute_bounded("INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','cccccccccccccccccccccccccccccccc','discord:99',?1,0)",params![legacy_payload], SCHEMA_BATCH_LIMIT).unwrap();
    let old_app_unban = r#"{"source":{"application":{"operation_id":"dddddddddddddddddddddddddddddddd"}},"action":"unban","outcome":"succeeded","subject_id":"78","actor_id":null,"occurred_at_ms":901,"reason":null,"evidence":"unavailable"}"#;
    raw.execute_bounded("INSERT INTO guild_cases(guild_id,enrollment_id,case_id,source_key,payload,deleted) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','dddddddddddddddddddddddddddddddd','app:dddddddddddddddddddddddddddddddd',?1,0)",params![old_app_unban], SCHEMA_BATCH_LIMIT).unwrap();
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
    assert!(
        store.get_member(scope.clone(), 78).await.unwrap().is_none(),
        "ambiguous historical application unban does not prove a ban"
    );
    assert_eq!(
        store
            .member_history(scope.clone(), 78, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        1,
        "case remains in the timeline"
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
    store.db.with_conn(|conn|{let mut tx=conn.transaction_primary_read()?;tx.ensure_mutation_receipt()?;tx.member_execute("UPDATE guild_cases SET deleted=1,payload=NULL WHERE guild_id=10 AND case_id='cccccccccccccccccccccccccccccccc'",())?;tx.commit()}).await.unwrap();
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
            tx.member_execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.member_execute("DELETE FROM guild_member_seed WHERE guild_id=10", ())?;
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
async fn long_request_lease_commits_member_and_audit_pages_after_ninety_seconds() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_000, 150_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Members, None, true),
            121_000,
        )
        .await
        .unwrap();

    // The separate audit recovery lane uses the same lease budget.
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.member_execute(
                "UPDATE guild_member_seed SET initial_completed_at_ms=121001 WHERE guild_id=10",
                (),
            )?;
            tx.member_execute(
                "UPDATE guild_member_sync SET phase='complete',status='complete' WHERE guild_id=10",
                (),
            )?;
            tx.commit()
        })
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 122_000)
        .await
        .unwrap();
    let audit = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 122_000, 150_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(scope, audit, vec![], (None, true), 242_000, 15_000)
        .await
        .unwrap();
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
    let by_id = store
        .list_members(
            scope.clone(),
            MemberQuery {
                q: Some("9007199254740993".into()),
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
    assert_eq!(by_id.members.len(), 1);
    assert_eq!(by_id.members[0].user_id, "9007199254740993");
    let by_mention = store
        .list_members(
            scope.clone(),
            MemberQuery {
                q: Some("<@!9007199254740993>".into()),
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
    assert_eq!(by_mention.members.len(), 1);
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
            tx.member_execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.member_execute("DELETE FROM guild_member_seed WHERE guild_id=10", ())?;
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
    let banned = store.get_member(scope.clone(), 77).await.unwrap().unwrap();
    assert!(banned.is_banned);
    assert!(!banned.is_current);
    assert_eq!(banned.departed_at_ms, None);
    let ended_unknown: bool = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=77",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert!(ended_unknown);
    let departed = store
        .depart_member(scope.clone(), 77, 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(departed.completed_stay_ms, Some(2_500));
    let ended_unknown: bool = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=77",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert!(!ended_unknown);
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
            tx.member_execute(
                "UPDATE guild_member_seed SET initial_completed_at_ms=2000,case_backfill_done=1 WHERE guild_id=10",
                (),
            )?;
            tx.member_execute(
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
    let ban_check = store
        .next_member_target_check(scope.clone(), 3_200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ban_check.kind, MemberVerifyKind::Ban);
    store
        .defer_member_target_check(scope.clone(), ban_check, 10_000)
        .await
        .unwrap();
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
