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
            tx.member_execute(
                "UPDATE guild_member_seed SET initial_completed_at_ms=2000 WHERE guild_id=10",
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
    assert_eq!(waiting.estimate_reason.as_deref(), Some("rate_limited"));
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
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
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
    assert_eq!(missing.observed_at_ms, Some(1_500));
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
    let departed = store
        .depart_member_checked(scope.clone(), 12, 2_200, None)
        .await
        .unwrap();
    let MemberDepartureDecision::Applied(Some(departed)) = departed else {
        panic!("a later matching remove should close the pending stay");
    };
    assert_eq!(departed.user_id, 12);
    assert_eq!(departed.joined_at_ms, Some(500));
    assert_eq!(departed.departed_at_ms, 2_200);
    assert_eq!(departed.completed_stay_ms, Some(1_700));
    let stay: (Option<i64>, bool) = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT departed_at_ms,ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=12",
                (),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(stay, (Some(2_200), false));
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
            .any(|e| e.kind == "ban_absent_state_observation"
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
    for (claim_at, committed_at) in [(3_100, 3_101), (18_101, 18_102)] {
        let claim = store
            .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), claim_at, 90_000)
            .await
            .unwrap()
            .unwrap();
        store
            .commit_member_audit_recovery_page(
                scope.clone(),
                claim,
                vec![],
                (None, true),
                committed_at,
                15_000,
            )
            .await
            .unwrap();
    }
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_000)
            .await
            .unwrap()
            .is_none(),
        "after audit recovery, gap cleanup first queues a member presence check"
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
async fn stalled_pagination_needs_attention_and_stale_controls_do_not_overwrite_it() {
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
            "pagination_stalled".into(),
            2_000,
            1_001,
        )
        .await
        .unwrap();
    let stalled = store
        .member_sync_status(scope.clone(), 42, 2_000)
        .await
        .unwrap();
    assert_eq!(stalled.status, "needs_attention");
    assert_eq!(stalled.status_reason.as_deref(), Some("pagination_stalled"));
    assert!(
        store
            .claim_member_sync(scope.clone(), 42, "worker".into(), 3_000, 90_000)
            .await
            .unwrap()
            .is_none()
    );
    let error = store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Pause,
            "stale-pause".into(),
            3_100,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<MemberSyncError>(),
        Some(MemberSyncError::Invalid)
    ));
    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 3_100)
            .await
            .unwrap()
            .status,
        "needs_attention"
    );
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "retry-stall".into(),
            3_200,
        )
        .await
        .unwrap();
    assert!(
        store
            .claim_member_sync(scope.clone(), 42, "worker".into(), 3_200, 90_000)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn retry_requeues_audit_pagination_stall_after_initial_complete() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 5_000)
        .await
        .unwrap();
    store.db.with_conn(|conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.member_execute(
            "UPDATE guild_member_seed SET initial_completed_at_ms=5000 WHERE guild_id=10",
            (),
        )?;
        tx.member_execute(
            "UPDATE guild_member_sync SET phase='complete',status='complete',completed_at_ms=5000,last_success_at_ms=5000 WHERE guild_id=10",
            (),
        )?;
        tx.member_execute(
            "INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,status_reason,generation,processed)
             VALUES(10,(SELECT enrollment_id FROM guild_member_seed WHERE guild_id=10 LIMIT 1),42,'audit-run',5000,'needs_attention','pagination_stalled',0,0)
             ON CONFLICT(guild_id) DO UPDATE SET status='needs_attention',status_reason='pagination_stalled',lease_owner=NULL,lease_until_ms=NULL",
            (),
        )?;
        tx.commit()
    }).await.unwrap();
    let stalled = store
        .member_sync_status(scope.clone(), 42, 5_500)
        .await
        .unwrap();
    assert!(!stalled.audit_recovery_pending);
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "retry-audit-stall".into(),
            6_000,
        )
        .await
        .unwrap();
    let status = store
        .member_sync_status(scope.clone(), 42, 6_000)
        .await
        .unwrap();
    assert_eq!(status.status, "complete");
    assert_eq!(status.audit_recovery_status.as_deref(), Some("queued"));
    assert_eq!(status.audit_recovery_reason, None);
    assert!(status.audit_recovery_next_attempt_at.unwrap_or(0) >= 6_000);
}

#[tokio::test]
async fn retry_does_not_resume_paused_reconciliation_for_stalled_audit() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 5_000)
        .await
        .unwrap();
    store.db.with_conn(|conn| {
        let mut tx = conn.transaction_primary_read()?;
        tx.ensure_mutation_receipt()?;
        tx.member_execute("UPDATE guild_member_seed SET initial_completed_at_ms=5000 WHERE guild_id=10", ())?;
        tx.member_execute("UPDATE guild_member_sync SET phase='complete',status='complete',completed_at_ms=5000 WHERE guild_id=10", ())?;
        tx.member_execute("INSERT INTO guild_member_audit_recovery(guild_id,enrollment_id,bot_id,run_id,started_at_ms,status,status_reason,generation,processed) VALUES(10,(SELECT enrollment_id FROM guild_member_seed WHERE guild_id=10),42,'audit-run',5000,'needs_attention','pagination_stalled',0,0)", ())?;
        tx.commit()
    }).await.unwrap();
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Reconcile,
            "reconcile".into(),
            6_000,
        )
        .await
        .unwrap();
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Pause,
            "pause".into(),
            6_001,
        )
        .await
        .unwrap();
    let before = store
        .member_sync_status(scope.clone(), 42, 6_002)
        .await
        .unwrap();
    assert_eq!(before.status, "paused");
    assert!(!before.allows_member_sync_retry());
    let error = store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "retry".into(),
            6_003,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<MemberSyncError>(),
        Some(MemberSyncError::Invalid)
    ));
    let after = store
        .member_sync_status(scope.clone(), 42, 6_003)
        .await
        .unwrap();
    assert_eq!(after.status, "paused");
    assert_eq!(
        after.audit_recovery_status.as_deref(),
        Some("needs_attention")
    );
    assert_eq!(after.progress_revision, before.progress_revision);
}

#[tokio::test]
async fn control_status_failure_rolls_back_instead_of_reporting_committed_failure() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    // A trigger changes the bot identity after the control update, so the
    // subsequent status projection fails. The control must roll back with it.
    store
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TRIGGER bad_pause_status AFTER UPDATE OF status ON guild_member_sync WHEN NEW.status='paused' BEGIN UPDATE guild_member_seed SET bot_id=999 WHERE guild_id=NEW.guild_id; END;",
                SCHEMA_BATCH_LIMIT,
            )
        })
        .await
        .unwrap();
    assert!(
        store
            .member_sync_action(
                scope.clone(),
                42,
                MemberSyncAction::Pause,
                "pause".into(),
                1_100
            )
            .await
            .is_err()
    );
    let (state, bot_id): (String, i64) = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT s.status,seed.bot_id FROM guild_member_sync s JOIN guild_member_seed seed USING(guild_id) WHERE s.guild_id=10",
                (),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "queued");
    assert_eq!(bot_id, 42);
}

#[tokio::test]
async fn retry_accepts_expired_running_lease_and_fences_old_worker() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let old_claim = store
        .claim_member_sync(scope.clone(), 42, "old-worker".into(), 1_000, 90_000)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 91_000)
            .await
            .unwrap()
            .status,
        "running"
    );
    let error = store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "active-lease".into(),
            91_000,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<MemberSyncError>(),
        Some(MemberSyncError::Invalid)
    ));

    let expired = store
        .member_sync_status(scope.clone(), 42, 91_001)
        .await
        .unwrap();
    assert_eq!(expired.status, "waiting");
    assert!(expired.allows_member_sync_retry());
    let retried = store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Retry,
            "expired-lease".into(),
            91_001,
        )
        .await
        .unwrap();
    assert_eq!(retried.status, "queued");

    let error = store
        .defer_member_sync(scope.clone(), old_claim, "transient".into(), 92_000, 91_001)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<MemberSyncError>(),
        Some(MemberSyncError::Stale)
    ));
    assert!(
        store
            .claim_member_sync(scope, 42, "new-worker".into(), 91_001, 90_000)
            .await
            .unwrap()
            .is_some()
    );
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
    store.db.with_conn(|conn|{let mut tx=conn.transaction_primary_read()?;tx.ensure_mutation_receipt()?;tx.member_execute("UPDATE guild_member_seed SET initial_completed_at_ms=5000 WHERE guild_id=10",())?;tx.member_execute("UPDATE guild_member_sync SET phase='complete',status='complete',lease_owner=NULL,lease_until_ms=NULL WHERE guild_id=10",())?;tx.commit()}).await.unwrap();
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

#[tokio::test]
async fn no_row_remove_fences_inflight_page_and_old_join_cannot_reappear() {
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
    assert!(matches!(
        store
            .depart_member_checked(scope.clone(), 61, 1_001, None)
            .await
            .unwrap(),
        MemberDepartureDecision::Applied(None)
    ));
    let mut old_page = page(MemberSyncPhase::Members, None, false);
    old_page.members = vec![observed(61, Some(500), 1_000)];
    old_page.next_cursor = Some("61".into());
    assert!(
        store
            .commit_member_page(scope.clone(), claim.clone(), old_page, 1_002)
            .await
            .unwrap_err()
            .downcast_ref::<MemberSyncError>()
            .is_some_and(|e| matches!(e, MemberSyncError::Stale))
    );
    store
        .defer_member_sync(
            scope.clone(),
            claim,
            "directory_changed".into(),
            16_002,
            1_002,
        )
        .await
        .unwrap();
    let retry = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut repeated = page(MemberSyncPhase::Members, None, false);
    repeated.members = vec![observed(61, Some(500), 16_002)];
    repeated.next_cursor = Some("61".into());
    store
        .commit_member_page(scope.clone(), retry, repeated, 16_003)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 61).await.unwrap().unwrap();
    assert!(!member.is_current);
    assert_eq!(member.departed_at_ms, Some(1_001));
    let check = store
        .next_member_target_check(scope.clone(), 16_003)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 61);
}

#[tokio::test]
async fn older_member_page_token_preserves_live_newer_stay() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(62, Some(3_000), 3_100))
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 5_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut stale = page(MemberSyncPhase::Members, None, false);
    stale.members = vec![observed(62, Some(500), 5_000)];
    stale.next_cursor = Some("62".into());
    store
        .commit_member_page(scope.clone(), claim, stale, 5_001)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 62).await.unwrap().unwrap();
    assert!(member.is_current);
    assert_eq!(member.joined_at_ms, Some(3_000));
    let check = store
        .next_member_target_check(scope.clone(), 5_001)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 62);
    assert_eq!(check.revision, member.revision);
}

#[tokio::test]
async fn confirmed_ban_page_closes_open_stay_without_leave_time() {
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
    let mut members = page(MemberSyncPhase::Members, None, true);
    members.members = vec![observed(63, Some(500), 1_000)];
    store
        .commit_member_page(scope.clone(), claim, members, 1_001)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, None, true);
    bans.bans = vec![MemberBanSnapshot {
        user_id: 63,
        username: None,
        global_name: None,
        reason: Some("confirmed".into()),
    }];
    store
        .commit_member_page(scope.clone(), claim, bans, 16_002)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 63).await.unwrap().unwrap();
    assert!(member.is_banned && member.ever_banned);
    assert!(!member.is_current);
    assert_eq!(member.joined_at_ms, Some(500));
    assert_eq!(member.departed_at_ms, None);
    let ended_unknown: bool = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=63",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert!(ended_unknown);
    let departed = store
        .depart_member(scope.clone(), 63, 17_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(departed.joined_at_ms, Some(500));
    assert_eq!(departed.completed_stay_ms, Some(16_500));
    let resolved = store.get_member(scope.clone(), 63).await.unwrap().unwrap();
    assert_eq!(resolved.departed_at_ms, Some(17_000));
    let ended_unknown: bool = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT ended_unknown FROM guild_member_stays WHERE guild_id=10 AND user_id=63",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert!(!ended_unknown);
}

#[tokio::test]
async fn remove_before_confirmed_ban_keeps_saved_departure() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(66, Some(500), 1_000))
        .await
        .unwrap();
    let departure = store
        .depart_member(scope.clone(), 66, 2_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(departure.completed_stay_ms, Some(1_500));
    assert!(store.observe_ban(scope.clone(), 66, 2_100).await.unwrap());
    let profile = store.get_member(scope.clone(), 66).await.unwrap().unwrap();
    assert!(!profile.is_current && profile.is_banned && profile.ever_banned);
    assert_eq!(profile.departed_at_ms, Some(2_000));
    assert!(
        store
            .depart_member(scope.clone(), 66, 2_200)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn known_count_has_fixed_preliminary_eta_only_during_recent_queued_initialization() {
    let (_f, store, scope) = ready().await;
    let initial = store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    assert_eq!(initial.phase_finish_at, None);
    store
        .set_initial_member_estimate(
            scope.clone(),
            42,
            MemberInitialEstimate {
                member_count: 200,
                page_size: 100,
                pace_ms: 15_000,
                active_guild_count: 3,
                now_ms: 1_001,
            },
        )
        .await
        .unwrap();
    let first = store
        .member_sync_status(scope.clone(), 42, 1_001)
        .await
        .unwrap();
    let finish = first.phase_finish_at.unwrap();
    assert_eq!(
        first.estimate_reason.as_deref(),
        Some("preliminary member-stage estimate; later stages have unknown totals")
    );
    let later = store
        .member_sync_status(scope.clone(), 42, 10_001)
        .await
        .unwrap();
    assert_eq!(later.phase_finish_at, Some(finish));
    assert!(later.phase_remaining_seconds.unwrap() < first.phase_remaining_seconds.unwrap());
    let stale = store
        .member_sync_status(scope.clone(), 42, 121_002)
        .await
        .unwrap();
    assert_eq!(stale.phase_finish_at, None);
    assert_eq!(stale.estimate_reason.as_deref(), Some("worker_unconfirmed"));
}

fn member_verification(
    check: &MemberTargetCheck,
    member: Option<MemberObservation>,
) -> MemberVerification {
    MemberVerification {
        user_id: check.user_id,
        revision: check.revision,
        kind: MemberVerifyKind::Member,
        present: member.is_some(),
        member,
        ban: None,
    }
}

#[tokio::test]
async fn tokenless_second_stay_leave_verifies_absence_then_returns_saved_duration_after_reopen() {
    for (replace_revision, remove_time) in [(true, 2_000), (false, 2_050), (false, 2_000)] {
        let (f, store, scope) = ready().await;
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        store
            .observe_member(scope.clone(), observed(64, Some(500), 1_000))
            .await
            .unwrap();
        store.depart_member(scope.clone(), 64, 1_100).await.unwrap();
        store
            .observe_member(scope.clone(), observed(64, Some(1_500), 1_600))
            .await
            .unwrap();
        assert!(matches!(
            store
                .depart_member_checked(scope.clone(), 64, 2_000, None)
                .await
                .unwrap(),
            MemberDepartureDecision::NeedsVerification
        ));
        let old = store
            .next_member_target_check(scope.clone(), 2_000)
            .await
            .unwrap()
            .unwrap();
        if replace_revision {
            let mut changed = observed(64, Some(1_500), 2_000);
            changed.username = Some(Some("changed profile".into()));
            store.observe_member(scope.clone(), changed).await.unwrap();
        }
        assert!(matches!(
            store
                .depart_member_checked(scope.clone(), 64, remove_time, None)
                .await
                .unwrap(),
            MemberDepartureDecision::NeedsVerification
        ));
        let verification = member_verification(&old, Some(observed(64, Some(1_500), 2_020)));
        let err = store
            .settle_member_target_check(scope.clone(), old.clone(), verification, 2_100)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<MemberSyncError>(),
            Some(MemberSyncError::Stale)
        ));
        let reopened = GuildDataStore::open(f.options(), store.expected.clone()).unwrap();
        let check = reopened
            .next_member_target_check(scope.clone(), 2_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(check.reason, "ambiguous_departure");
        assert!(check.expected_stay_id.is_some());
        assert_eq!(check.expected_stay_id, old.expected_stay_id);
        assert_eq!(check.requested_at_ms, 2_000);
        let departure = reopened
            .settle_member_target_check(
                scope.clone(),
                check.clone(),
                member_verification(&check, None),
                2_100,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(departure.joined_at_ms, Some(1_500));
        assert_eq!(departure.departed_at_ms, 2_000);
        assert_eq!(departure.completed_stay_ms, Some(500));
        assert!(
            !reopened
                .get_member(scope.clone(), 64)
                .await
                .unwrap()
                .unwrap()
                .is_current
        );
        assert!(
            reopened
                .next_member_target_check(scope.clone(), 2_100)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn tokenless_second_stay_leave_preserves_present_or_newer_stay_without_orphan_check() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(65, Some(500), 1_000))
        .await
        .unwrap();
    store.depart_member(scope.clone(), 65, 1_100).await.unwrap();
    store
        .observe_member(scope.clone(), observed(65, Some(1_500), 1_600))
        .await
        .unwrap();
    assert!(matches!(
        store
            .depart_member_checked(scope.clone(), 65, 2_000, None)
            .await
            .unwrap(),
        MemberDepartureDecision::NeedsVerification
    ));
    let check = store
        .next_member_target_check(scope.clone(), 2_000)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .settle_member_target_check(
                scope.clone(),
                check.clone(),
                member_verification(&check, Some(observed(65, Some(1_500), 2_100))),
                2_100
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_member(scope.clone(), 65)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
    assert!(
        store
            .next_member_target_check(scope.clone(), 2_100)
            .await
            .unwrap()
            .is_none()
    );

    assert!(matches!(
        store
            .depart_member_checked(scope.clone(), 65, 2_200, None)
            .await
            .unwrap(),
        MemberDepartureDecision::NeedsVerification
    ));
    let stale = store
        .next_member_target_check(scope.clone(), 2_200)
        .await
        .unwrap()
        .unwrap();
    store
        .observe_member(scope.clone(), observed(65, Some(3_000), 3_100))
        .await
        .unwrap();
    assert!(
        store
            .settle_member_target_check(
                scope.clone(),
                stale.clone(),
                member_verification(&stale, None),
                3_200
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_member(scope.clone(), 65)
            .await
            .unwrap()
            .unwrap()
            .joined_at_ms,
        Some(3_000)
    );
    assert!(
        store
            .next_member_target_check(scope.clone(), 3_200)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retained_short_second_stay_confirms_ban_then_saved_departure() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(67, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 67, 1_100).await.unwrap();
    let first = store
        .depart_member(scope.clone(), 67, 1_200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.completed_stay_ms, Some(700));
    store.observe_unban(scope.clone(), 67, 1_300).await.unwrap();
    store
        .observe_member(scope.clone(), observed(67, Some(1_500), 1_600))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 67, 1_700).await.unwrap();
    let before = store.get_member(scope.clone(), 67).await.unwrap().unwrap();
    assert!(!before.is_current && before.is_banned);
    assert_eq!(before.departed_at_ms, None);
    assert!(matches!(
        store
            .depart_member_checked(scope.clone(), 67, 1_800, None)
            .await
            .unwrap(),
        MemberDepartureDecision::NeedsVerification
    ));
    let check = store
        .next_member_target_check(scope.clone(), 1_800)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.reason, "ambiguous_departure");
    let second = store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            member_verification(&check, None),
            1_900,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.joined_at_ms, Some(1_500));
    assert_eq!(second.departed_at_ms, 1_800);
    assert_eq!(second.completed_stay_ms, Some(300));
    let after = store.get_member(scope.clone(), 67).await.unwrap().unwrap();
    assert!(!after.is_current && after.is_banned && after.ever_banned);
    assert_eq!(after.departed_at_ms, Some(1_800));
}

#[tokio::test]
async fn case_projection_keeps_noop_and_ambiguous_app_unban_in_history_without_retention() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let data = |source, action, outcome, subject_id| CaseData {
        source,
        action,
        outcome,
        subject_id,
        actor_id: Some(42),
        occurred_at_ms: 2_000,
        reason: None,
        evidence: CaseEvidence::Unavailable,
    };
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            data(
                CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                CaseAction::Unban,
                CaseOutcome::Attempted,
                71,
            ),
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            data(
                CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                CaseAction::Unban,
                CaseOutcome::Succeeded,
                72,
            ),
            CaseLimits::default(),
        )
        .await
        .unwrap();
    assert!(store.get_member(scope.clone(), 71).await.unwrap().is_none());
    assert!(store.get_member(scope.clone(), 72).await.unwrap().is_none());
    assert_eq!(
        store
            .member_history(scope.clone(), 71, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    assert_eq!(
        store
            .member_history(scope.clone(), 72, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            data(
                CaseSource::DiscordAudit { entry_id: 79 },
                CaseAction::Unban,
                CaseOutcome::Succeeded,
                73,
            ),
            CaseLimits::default(),
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 73)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            data(
                CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                CaseAction::Ban,
                CaseOutcome::Succeeded,
                74,
            ),
            CaseLimits::default(),
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 74)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
}

#[tokio::test]
async fn eligible_first_status_is_queued_but_admitted_missing_metadata_fails_closed() {
    let (_f, store, scope) = ready().await;
    let first = store
        .member_sync_status(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    assert_eq!(first.status, "queued");
    assert_eq!(first.run_kind.as_deref(), Some("initial"));
    assert_eq!(
        first.status_reason.as_deref(),
        Some("awaiting first bot initialization")
    );
    assert_eq!(first.run_id, None);
    let admitted = store
        .initialize_member_directory(scope.clone(), 42, 1_100)
        .await
        .unwrap();
    assert_eq!(admitted.status, "queued");
    assert!(admitted.run_id.is_some());
    assert_eq!(
        store
            .persisted_member_sync_bot_id(scope.clone())
            .await
            .unwrap(),
        Some(42)
    );
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.member_execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.commit()
        })
        .await
        .unwrap();
    let corrupt = store
        .member_sync_status(scope.clone(), 42, 1_200)
        .await
        .unwrap();
    assert_eq!(corrupt.status, "needs_attention");
    assert_eq!(corrupt.run_id, None);
    assert_eq!(
        store
            .initialize_member_directory(scope.clone(), 42, 1_300)
            .await
            .unwrap()
            .status,
        "needs_attention"
    );
}

#[tokio::test]
async fn live_rejoin_clears_stale_current_ban_and_checks_only_affected_user() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(81, Some(500), 1_000))
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 81, 1_100).await.unwrap();
    store
        .observe_member(scope.clone(), observed(81, Some(1_500), 1_600))
        .await
        .unwrap();
    let profile = store.get_member(scope.clone(), 81).await.unwrap().unwrap();
    assert!(profile.is_current && !profile.is_banned && profile.ever_banned);
    let check = store
        .next_member_target_check(scope.clone(), 1_600)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.kind, MemberVerifyKind::Ban);
    assert_eq!(check.user_id, 81);
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 81,
                revision: check.revision,
                kind: MemberVerifyKind::Ban,
                present: false,
                member: None,
                ban: None,
            },
            1_700,
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 81)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
    assert!(
        store
            .next_member_target_check(scope.clone(), 1_700)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn member_page_rejoin_clears_stale_current_ban() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 82, 1_100).await.unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_200, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut members = page(MemberSyncPhase::Members, None, false);
    members.members = vec![observed(82, Some(1_500), 1_600)];
    members.next_cursor = Some("82".into());
    store
        .commit_member_page(scope.clone(), claim, members, 1_601)
        .await
        .unwrap();
    let profile = store.get_member(scope.clone(), 82).await.unwrap().unwrap();
    assert!(profile.is_current && !profile.is_banned && profile.ever_banned);
    let check = store
        .next_member_target_check(scope.clone(), 1_601)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((check.kind, check.user_id), (MemberVerifyKind::Ban, 82));
}

#[tokio::test]
async fn old_stay_payload_cannot_replace_new_stay_name_or_search() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let mut first = observed(83, Some(500), 1_000);
    first.username = Some(Some("Old Name".into()));
    store.observe_member(scope.clone(), first).await.unwrap();
    store.depart_member(scope.clone(), 83, 1_100).await.unwrap();
    let mut newer = observed(83, Some(1_500), 1_600);
    newer.username = Some(Some("New Name".into()));
    store.observe_member(scope.clone(), newer).await.unwrap();
    let mut late = observed(83, Some(500), 1_700);
    late.username = Some(Some("Old Alias".into()));
    store.observe_member(scope.clone(), late).await.unwrap();
    let profile = store.get_member(scope.clone(), 83).await.unwrap().unwrap();
    assert_eq!(profile.username.as_deref(), Some("New Name"));
    assert_eq!(profile.joined_at_ms, Some(1_500));
    let query = |q: &str| MemberQuery {
        q: Some(q.into()),
        status: MemberStatusFilter::Current,
        joined_from_ms: None,
        joined_to_ms: None,
        sort: MemberSort::JoinedNewest,
        after: None,
        limit: 10,
    };
    assert_eq!(
        store
            .list_members(scope.clone(), query("new name"))
            .await
            .unwrap()
            .members
            .len(),
        1
    );
    assert!(
        store
            .list_members(scope.clone(), query("old alias"))
            .await
            .unwrap()
            .members
            .is_empty()
    );
}

#[tokio::test]
async fn known_rate_limit_has_retry_deadline_and_recent_anchored_finish() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .set_initial_member_estimate(
            scope.clone(),
            42,
            MemberInitialEstimate {
                member_count: 300,
                page_size: 100,
                pace_ms: 15_000,
                active_guild_count: 2,
                now_ms: 1_001,
            },
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 1_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut first = page(MemberSyncPhase::Members, None, false);
    first.members = vec![observed(84, Some(500), 1_001)];
    first.next_cursor = Some("84".into());
    first.total_estimate = Some(300);
    first.active_guild_count = 2;
    store
        .commit_member_page(scope.clone(), claim, first, 1_002)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .defer_member_sync(scope.clone(), claim, "rate_limited".into(), 50_000, 16_003)
        .await
        .unwrap();
    let waiting = store
        .member_sync_status(scope.clone(), 42, 16_003)
        .await
        .unwrap();
    assert_eq!(waiting.status, "waiting");
    assert_eq!(waiting.status_reason.as_deref(), Some("rate_limited"));
    assert_eq!(waiting.estimate_reason.as_deref(), Some("rate_limited"));
    assert_eq!(waiting.next_attempt_at, Some(50_000));
    assert_eq!(waiting.last_progress_at, Some(1_002));
    assert_eq!(waiting.as_of, 16_003);
    let finish = waiting.phase_finish_at.unwrap();
    assert!(finish > 50_000);
    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 17_000)
            .await
            .unwrap()
            .phase_finish_at,
        Some(finish)
    );
    let stale = store
        .member_sync_status(scope.clone(), 42, 200_000)
        .await
        .unwrap();
    assert_eq!(stale.phase_finish_at, None);
}

#[tokio::test]
async fn undated_ban_snapshot_adds_one_state_observation_without_action_case() {
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
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Members, None, true),
            1_001,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, None, true);
    bans.bans = vec![MemberBanSnapshot {
        user_id: 86,
        username: None,
        global_name: None,
        reason: None,
    }];
    store
        .commit_member_page(scope.clone(), claim, bans, 16_002)
        .await
        .unwrap();
    let history = store
        .member_history(scope.clone(), 86, None, 50)
        .await
        .unwrap();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].kind, "ban_present_state_observation");
    assert_eq!(history.entries[0].occurred_at_ms, None);
    assert_eq!(history.entries[0].observed_at_ms, Some(16_002));
    assert_eq!(history.entries[0].case_id, None);
    store
        .queue_member_target_check(
            scope.clone(),
            86,
            MemberVerifyKind::Ban,
            "confirm_snapshot",
            17_000,
        )
        .await
        .unwrap();
    let check = store
        .next_member_target_check(scope.clone(), 17_000)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 86,
                revision: check.revision,
                kind: MemberVerifyKind::Ban,
                present: true,
                member: None,
                ban: Some(MemberBanSnapshot {
                    user_id: 86,
                    username: None,
                    global_name: None,
                    reason: None,
                }),
            },
            17_001,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .member_history(scope.clone(), 86, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
}

async fn departed_member(store: &GuildDataStore, scope: &GuildDataScope, user_id: i64) {
    complete_for_checks(store, scope).await;
    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id,
                username: Some(Some("Leaver".into())),
                global_name: None,
                nickname: None,
                joined_at_ms: Some(1_500),
                observed_at_ms: 2_000,
                is_join: true,
            },
        )
        .await
        .unwrap();
    store
        .depart_member(scope.clone(), user_id, 2_500)
        .await
        .unwrap();
}

#[tokio::test]
async fn confirmed_unban_without_prior_local_ban_retains_departed_member() {
    let (_f, store, scope) = ready().await;
    departed_member(&store, &scope, 62).await;
    let before = store.get_member(scope.clone(), 62).await.unwrap().unwrap();
    assert!(!before.is_banned && !before.ever_banned);
    store.observe_unban(scope.clone(), 62, 2_600).await.unwrap();
    let retained = store.get_member(scope.clone(), 62).await.unwrap().unwrap();
    assert!(!retained.is_banned && retained.ever_banned);
    assert_eq!(retained.joined_at_ms, Some(1_500));
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_000)
            .await
            .unwrap()
            .is_none()
    );
}
