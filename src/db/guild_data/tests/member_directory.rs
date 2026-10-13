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
        ban_attempt_tag: None,
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
async fn stalled_initial_import_checks_departures_then_preserves_stays_until_history_is_ready() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    for user_id in [85, 86] {
        store
            .observe_member(scope.clone(), observed(user_id, Some(500), 1_000))
            .await
            .unwrap();
        store
            .depart_member(scope.clone(), user_id, 1_100)
            .await
            .unwrap();
    }
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 301_101)
            .await
            .unwrap()
            .is_none()
    );
    for (user_id, checked_at) in [(85, 301_102), (86, 301_104)] {
        let check = store
            .next_member_target_check(scope.clone(), checked_at - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            check.user_id, user_id,
            "a checked row cannot starve the next departure"
        );
        assert_eq!(check.reason, "cleanup_before_initial_coverage");
        store
            .settle_member_target_check(
                scope.clone(),
                check.clone(),
                MemberVerification {
                    user_id,
                    revision: check.revision,
                    kind: MemberVerifyKind::Member,
                    present: false,
                    member: None,
                    ban: None,
                },
                checked_at,
            )
            .await
            .unwrap();
        assert!(
            store
                .next_member_cleanup_candidate(scope.clone(), checked_at + 1)
                .await
                .unwrap()
                .is_none()
        );
    }
    let saved = store.get_member(scope.clone(), 85).await.unwrap().unwrap();
    assert_eq!(saved.joined_at_ms, Some(500));
    assert!(
        store
            .settle_member_cleanup(
                scope.clone(),
                MemberCleanupCandidate {
                    user_id: 85,
                    revision: saved.revision,
                },
                false,
                301_106,
            )
            .await
            .is_err(),
        "a stale candidate cannot bypass the initial-history gate"
    );

    let members = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 301_106, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            members,
            page(MemberSyncPhase::Members, None, true),
            301_107,
        )
        .await
        .unwrap();
    let bans = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 316_107, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            bans,
            page(MemberSyncPhase::Bans, None, true),
            316_108,
        )
        .await
        .unwrap();
    let ban_audit = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 331_108, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut ban_page = page(MemberSyncPhase::Audit, None, true);
    ban_page.audits = vec![MemberAuditSnapshot {
        entry_id: 101,
        user_id: 85,
        executor_id: Some(42),
        action: MemberAuditAction::Ban,
        occurred_at_ms: 100,
        reason: None,
    }];
    store
        .commit_member_page(scope.clone(), ban_audit, ban_page, 331_109)
        .await
        .unwrap();
    let unban_audit = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 346_109, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut unban_page = page(MemberSyncPhase::Audit, Some("unban:".into()), true);
    unban_page.audits = vec![MemberAuditSnapshot {
        entry_id: 102,
        user_id: 85,
        executor_id: Some(42),
        action: MemberAuditAction::Unban,
        occurred_at_ms: 200,
        reason: None,
    }];
    let waiting = store
        .commit_member_page(scope.clone(), unban_audit, unban_page, 346_110)
        .await
        .unwrap();
    assert_eq!(waiting.status, "waiting");
    assert!(
        waiting.initial_completed_at.is_none(),
        "local case backfill is pending"
    );
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 346_111)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    let completed = store
        .member_sync_status(scope.clone(), 42, 346_112)
        .await
        .unwrap();
    assert!(completed.initial_completed_at.is_some());
    assert_eq!(completed.status, "complete");
    let retained = store.get_member(scope.clone(), 85).await.unwrap().unwrap();
    assert!(retained.ever_banned);
    assert!(!retained.is_banned);
    assert_eq!(retained.joined_at_ms, Some(500));
    let history = store
        .member_history(scope.clone(), 85, None, 50)
        .await
        .unwrap();
    assert!(history.entries.iter().any(|entry| entry.kind == "ban"));
    assert!(history.entries.iter().any(|entry| entry.kind == "unban"));
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 346_112)
            .await
            .unwrap()
            .is_some_and(|candidate| candidate.user_id == 86)
    );
    let ordinary = store
        .next_member_cleanup_candidate(scope.clone(), 346_112)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_cleanup(scope.clone(), ordinary, false, 346_113)
        .await
        .unwrap();
    assert!(store.get_member(scope.clone(), 86).await.unwrap().is_none());
    assert!(store.get_member(scope.clone(), 85).await.unwrap().is_some());
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

async fn finish_empty_reconcile(store: &GuildDataStore, scope: &GuildDataScope, start: i64) {
    for (index, phase) in [
        MemberSyncPhase::Members,
        MemberSyncPhase::Bans,
        MemberSyncPhase::Verify,
        MemberSyncPhase::Audit,
        MemberSyncPhase::Audit,
    ]
    .into_iter()
    .enumerate()
    {
        let now = start + index as i64 * 15_001;
        let claim = store
            .claim_member_sync(scope.clone(), 42, "worker".into(), now, 90_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.phase, phase);
        store
            .commit_member_page(
                scope.clone(),
                claim.clone(),
                page(phase, claim.cursor.clone(), true),
                now + 1,
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn newer_gap_survives_reconcile_final_and_last_summary_survives_next_run() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store
        .backfill_member_case_index(scope.clone(), 42, 100)
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "first_gap", 3_000)
        .await
        .unwrap();
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Reconcile,
            "first-reconcile".into(),
            3_001,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 3_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Members, None, true),
            3_002,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 18_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Bans, None, true),
            18_003,
        )
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "newer_gap", 18_004)
        .await
        .unwrap();
    for (index, phase) in [
        MemberSyncPhase::Verify,
        MemberSyncPhase::Audit,
        MemberSyncPhase::Audit,
    ]
    .into_iter()
    .enumerate()
    {
        let now = 33_004 + index as i64 * 15_001;
        let claim = store
            .claim_member_sync(scope.clone(), 42, "worker".into(), now, 90_000)
            .await
            .unwrap()
            .unwrap();
        store
            .commit_member_page(
                scope.clone(),
                claim.clone(),
                page(phase, claim.cursor.clone(), true),
                now + 1,
            )
            .await
            .unwrap();
    }
    let completed = store
        .member_sync_status(scope.clone(), 42, 63_007)
        .await
        .unwrap();
    assert_eq!(completed.status, "complete");
    assert!(completed.reconciliation_needed);
    assert_eq!(
        completed.reconciliation_reason.as_deref(),
        Some("newer_gap")
    );
    assert_eq!(completed.last_success_at, Some(63_007));
    assert_eq!(completed.last_success_processed, Some(completed.processed));
    let old_summary = (
        completed.last_success_at,
        completed.last_success_processed,
        completed.last_success_inserted,
        completed.last_success_updated,
        completed.last_success_unchanged,
    );
    let next = store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Reconcile,
            "second-reconcile".into(),
            80_000,
        )
        .await
        .unwrap();
    assert_eq!(next.processed, 0);
    assert_eq!(
        (
            next.last_success_at,
            next.last_success_processed,
            next.last_success_inserted,
            next.last_success_updated,
            next.last_success_unchanged
        ),
        old_summary
    );
    finish_empty_reconcile(&store, &scope, 80_000).await;
    assert!(
        !store
            .member_sync_status(scope.clone(), 42, 140_005)
            .await
            .unwrap()
            .reconciliation_needed
    );
}

#[tokio::test]
async fn delayed_case_backfill_finishes_run_but_cannot_clear_newer_gap() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store
        .mark_member_reconciliation_needed(scope.clone(), "first_gap", 3_000)
        .await
        .unwrap();
    store
        .member_sync_action(
            scope.clone(),
            42,
            MemberSyncAction::Reconcile,
            "backfill-reconcile".into(),
            3_001,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 3_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Members, None, true),
            3_002,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 18_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Bans, None, true),
            18_003,
        )
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "newer_gap", 18_004)
        .await
        .unwrap();
    for (index, phase) in [
        MemberSyncPhase::Verify,
        MemberSyncPhase::Audit,
        MemberSyncPhase::Audit,
    ]
    .into_iter()
    .enumerate()
    {
        let now = 33_004 + index as i64 * 15_001;
        let claim = store
            .claim_member_sync(scope.clone(), 42, "worker".into(), now, 90_000)
            .await
            .unwrap()
            .unwrap();
        store
            .commit_member_page(
                scope.clone(),
                claim.clone(),
                page(phase, claim.cursor.clone(), true),
                now + 1,
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 63_007)
            .await
            .unwrap()
            .status,
        "waiting"
    );
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    let done = store
        .member_sync_status(scope.clone(), 42, 63_008)
        .await
        .unwrap();
    assert_eq!(done.status, "complete");
    assert!(done.reconciliation_needed);
    assert!(done.last_success_at.is_some());
    assert_eq!(done.last_success_processed, Some(done.processed));
}

#[tokio::test]
async fn version_nine_upgrade_preserves_completion_and_unknown_time_history() {
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
        SNAPSHOT_SCHEMA,
        MEMBER_SCHEMA,
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
        "INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
        UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10;
        INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,initial_completed_at_ms,reconciliation_needed,gap_at_ms) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,1000,2000,1,2100);
        INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,processed,inserted,updated,unchanged,started_at_ms,last_success_at_ms) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,'cccccccccccccccccccccccccccccccc','initial','complete','complete',3,1,1,1,1000,2000);
        INSERT INTO guild_member_state_observations(guild_id,enrollment_id,observation_id,user_id,kind,observed_at_ms) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','dddddddddddddddddddddddddddddddd',87,'ban_absent',1500);",
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    raw.pragma_update(None, "user_version", 10).unwrap();
    drop(raw);
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
    let status = store
        .member_sync_status(scope.clone(), 42, 3_000)
        .await
        .unwrap();
    assert_eq!(status.initial_completed_at, Some(2_000));
    assert!(status.reconciliation_needed);
    assert_eq!(
        (
            status.last_success_processed,
            status.last_success_inserted,
            status.last_success_updated,
            status.last_success_unchanged
        ),
        (Some(3), Some(1), Some(1), Some(1))
    );
    let history = store
        .member_history(scope.clone(), 87, None, 50)
        .await
        .unwrap();
    assert_eq!(history.entries[0].kind, "ban_absent_state_observation");
    assert_eq!(history.entries[0].occurred_at_ms, None);
    let version: i64 = store
        .db
        .with_conn(|conn| conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?)))
        .await
        .unwrap();
    assert_eq!(version, VERSION);
}

#[tokio::test]
async fn version_ten_upgrade_preserves_existing_run_and_marks_old_phase_counts_unknown() {
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
        SNAPSHOT_SCHEMA,
        MEMBER_SCHEMA,
        MEMBER_STATUS_SCHEMA,
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
        "INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
        UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10;
        INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,initial_completed_at_ms,case_backfill_done) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,1000,2000,1);
        INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,processed,inserted,updated,unchanged,started_at_ms,last_success_at_ms) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,'cccccccccccccccccccccccccccccccc','initial','complete','complete',3,1,1,1,1000,2000);",
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    raw.pragma_update(None, "user_version", 11).unwrap();
    drop(raw);
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
    let status = store.member_sync_status(scope, 42, 3_000).await.unwrap();
    assert_eq!(
        status.run_id.as_deref(),
        Some("cccccccccccccccccccccccccccccccc")
    );
    assert_eq!(status.last_success_at, Some(2_000));
    assert_eq!(status.ban_processed, None);
    assert_eq!(status.verify_processed, None);
    assert_eq!(status.audit_processed, None);
    assert!(status.local_history_complete);
    let version: i64 = store
        .db
        .with_conn(|conn| conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?)))
        .await
        .unwrap();
    assert_eq!(version, VERSION);
}

#[tokio::test]
async fn failed_historical_ban_does_not_hide_confirmed_undated_ban_state() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .record_case(
            scope.clone(),
            CaseId::new(),
            CaseData {
                ban_attempt_tag: None,
                source: CaseSource::Application {
                    operation_id: CaseId::new(),
                },
                action: CaseAction::Ban,
                outcome: CaseOutcome::Failed,
                subject_id: 88,
                actor_id: Some(42),
                occurred_at_ms: 900,
                reason: None,
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
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
        user_id: 88,
        username: None,
        global_name: None,
        reason: None,
    }];
    store
        .commit_member_page(scope.clone(), claim, bans, 16_002)
        .await
        .unwrap();
    let history = store
        .member_history(scope.clone(), 88, None, 50)
        .await
        .unwrap();
    assert!(
        history
            .entries
            .iter()
            .any(|e| e.kind == "ban" && e.outcome.as_deref() == Some("failed"))
    );
    assert!(
        history
            .entries
            .iter()
            .any(|e| e.kind == "ban_present_state_observation"
                && e.occurred_at_ms.is_none()
                && e.observed_at_ms == Some(16_002)
                && e.case_id.is_none())
    );
    assert!(
        store
            .get_member(scope.clone(), 88)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
}

#[tokio::test]
async fn admitted_missing_sync_job_rejects_controls_without_reseeding() {
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
            tx.member_execute("DELETE FROM guild_member_sync WHERE guild_id=10", ())?;
            tx.commit()
        })
        .await
        .unwrap();
    for (index, action) in [
        MemberSyncAction::Pause,
        MemberSyncAction::Resume,
        MemberSyncAction::Retry,
        MemberSyncAction::Reconcile,
    ]
    .into_iter()
    .enumerate()
    {
        let error = store
            .member_sync_action(
                scope.clone(),
                42,
                action,
                format!("missing-{index}"),
                1_100 + index as i64,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<MemberSyncError>(),
            Some(MemberSyncError::MissingJob)
        ));
    }
    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 1_200)
            .await
            .unwrap()
            .status,
        "needs_attention"
    );
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
async fn equal_millisecond_opposite_ban_events_need_persisted_target_check() {
    let (f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 89, 2_000).await.unwrap();
    store.observe_unban(scope.clone(), 89, 2_000).await.unwrap();
    assert!(
        store
            .get_member(scope.clone(), 89)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
    let reopened = GuildDataStore::open(f.options(), store.expected.clone()).unwrap();
    let check = reopened
        .next_member_target_check(scope.clone(), 2_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.kind, MemberVerifyKind::Ban);
    assert_eq!(check.reason, "ban_order_uncertain");
    reopened
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 89,
                revision: check.revision,
                kind: MemberVerifyKind::Ban,
                present: false,
                member: None,
                ban: None,
            },
            2_100,
        )
        .await
        .unwrap();
    assert!(
        !reopened
            .get_member(scope.clone(), 89)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
    reopened
        .observe_unban(scope.clone(), 90, 3_000)
        .await
        .unwrap();
    reopened
        .observe_ban(scope.clone(), 90, 3_000)
        .await
        .unwrap();
    let check = reopened
        .next_member_target_check(scope.clone(), 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.reason, "ban_order_uncertain");
    reopened
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 90,
                revision: check.revision,
                kind: MemberVerifyKind::Ban,
                present: true,
                member: None,
                ban: Some(MemberBanSnapshot {
                    user_id: 90,
                    username: None,
                    global_name: None,
                    reason: None,
                }),
            },
            3_100,
        )
        .await
        .unwrap();
    assert!(
        reopened
            .get_member(scope.clone(), 90)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
    for (user_id, first_id, second_id) in [(89, 189, 190), (90, 191, 192)] {
        for (entry_id, action) in [(first_id, CaseAction::Ban), (second_id, CaseAction::Unban)] {
            reopened
                .record_case(
                    scope.clone(),
                    CaseId::new(),
                    CaseData {
                        ban_attempt_tag: None,
                        source: CaseSource::DiscordAudit { entry_id },
                        action,
                        outcome: CaseOutcome::Succeeded,
                        subject_id: user_id,
                        actor_id: Some(42),
                        occurred_at_ms: if user_id == 89 { 2_000 } else { 3_000 },
                        reason: None,
                        evidence: CaseEvidence::Unavailable,
                    },
                    CaseLimits::default(),
                )
                .await
                .unwrap();
        }
        let history = reopened
            .member_history(scope.clone(), user_id, None, 50)
            .await
            .unwrap();
        let cases: Vec<_> = history
            .entries
            .iter()
            .filter(|entry| entry.case_id.is_some())
            .collect();
        assert_eq!(
            cases.len(),
            2,
            "opposite equal-time actions keep distinct cases"
        );
        assert!(cases.iter().any(|entry| entry.kind == "ban"));
        assert!(cases.iter().any(|entry| entry.kind == "unban"));
        assert_ne!(cases[0].case_id, cases[1].case_id);
    }
}

#[tokio::test]
async fn measured_second_member_page_accounts_for_competing_paced_check() {
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
                member_count: 3,
                page_size: 1,
                pace_ms: 15_000,
                active_guild_count: 1,
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
    first.members = vec![observed(91, Some(500), 1_001)];
    first.next_cursor = Some("91".into());
    first.total_estimate = Some(3);
    first.page_size = 1;
    store
        .commit_member_page(scope.clone(), claim, first, 1_002)
        .await
        .unwrap();
    let first_eta = store
        .member_sync_status(scope.clone(), 42, 1_002)
        .await
        .unwrap()
        .phase_finish_at
        .unwrap();
    assert_eq!(first_eta, 61_002);
    store
        .queue_member_target_check(
            scope.clone(),
            91,
            MemberVerifyKind::Member,
            "paced_check",
            16_002,
        )
        .await
        .unwrap();
    let check = store
        .next_member_target_check(scope.clone(), 16_002)
        .await
        .unwrap()
        .unwrap();
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 91,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present: true,
                member: Some(observed(91, Some(500), 16_003)),
                ban: None,
            },
            16_003,
        )
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 31_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut second = page(MemberSyncPhase::Members, Some("91".into()), false);
    second.members = vec![observed(92, Some(1_500), 31_002)];
    second.next_cursor = Some("92".into());
    second.total_estimate = Some(3);
    second.page_size = 1;
    store
        .commit_member_page(scope.clone(), claim, second, 31_003)
        .await
        .unwrap();
    let measured: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT member_page_interval_ms FROM guild_member_sync WHERE guild_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(measured, 30_001);
    let status = store
        .member_sync_status(scope.clone(), 42, 31_003)
        .await
        .unwrap();
    assert_eq!(status.member_processed, Some(2));
    // The next request is due at 46_003; one member page and the terminal
    // request each take the measured 30_001 ms cycle, including the check.
    assert_eq!(status.phase_finish_at, Some(106_005));
    assert_eq!(
        store
            .member_sync_status(scope.clone(), 42, 32_000)
            .await
            .unwrap()
            .phase_finish_at,
        status.phase_finish_at
    );
}

#[tokio::test]
async fn phase_counts_and_local_history_wait_reflect_committed_work() {
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
    members.members = vec![
        observed(95, Some(500), 1_000),
        observed(96, Some(500), 1_000),
    ];
    store
        .commit_member_page(scope.clone(), claim, members, 1_001)
        .await
        .unwrap();
    let after_members = store
        .member_sync_status(scope.clone(), 42, 1_001)
        .await
        .unwrap();
    assert_eq!(after_members.phase.as_deref(), Some("bans"));
    assert_eq!(after_members.member_processed, Some(2));
    assert_eq!(after_members.ban_processed, Some(0));
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, None, true);
    bans.bans = vec![MemberBanSnapshot {
        user_id: 97,
        username: None,
        global_name: None,
        reason: None,
    }];
    store
        .commit_member_page(scope.clone(), claim, bans, 16_002)
        .await
        .unwrap();
    let after_bans = store
        .member_sync_status(scope.clone(), 42, 16_002)
        .await
        .unwrap();
    assert_eq!(after_bans.phase.as_deref(), Some("audit"));
    assert_eq!(after_bans.ban_processed, Some(1));
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 31_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut audits = page(MemberSyncPhase::Audit, None, true);
    audits.audits = vec![MemberAuditSnapshot {
        entry_id: 100,
        user_id: 97,
        executor_id: Some(42),
        action: MemberAuditAction::Ban,
        occurred_at_ms: 10_000,
        reason: None,
    }];
    store
        .commit_member_page(scope.clone(), claim, audits, 31_003)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 46_003, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_page(
            scope.clone(),
            claim,
            page(MemberSyncPhase::Audit, Some("unban:".into()), true),
            46_004,
        )
        .await
        .unwrap();
    let pending = store
        .member_sync_status(scope.clone(), 42, 46_004)
        .await
        .unwrap();
    assert_eq!(pending.phase.as_deref(), Some("complete"));
    assert_eq!(pending.status, "waiting");
    assert_eq!(pending.audit_processed, Some(1));
    assert_eq!(pending.verify_processed, Some(0));
    assert!(!pending.local_history_complete);
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    let done = store
        .member_sync_status(scope.clone(), 42, 46_005)
        .await
        .unwrap();
    assert!(done.local_history_complete);
    assert_eq!(done.status, "complete");
    assert_eq!(
        (
            done.member_processed,
            done.ban_processed,
            done.audit_processed
        ),
        (Some(2), Some(1), Some(1))
    );
}

#[tokio::test]
async fn pending_audit_gap_blocks_cleanup_then_preserves_recovered_ban_history() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    for user in [98, 99] {
        store
            .observe_member(scope.clone(), observed(user, Some(500), 1_000))
            .await
            .unwrap();
        store
            .depart_member(scope.clone(), user, 1_100)
            .await
            .unwrap();
    }
    let pre_gap = store
        .next_member_cleanup_candidate(scope.clone(), 301_100)
        .await
        .unwrap()
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 301_101)
        .await
        .unwrap();
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 301_102)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .settle_member_cleanup(scope.clone(), pre_gap, false, 301_102)
            .await
            .is_err()
    );
    let claim = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 301_101, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            claim,
            vec![MemberAuditSnapshot {
                entry_id: 101,
                user_id: 98,
                executor_id: Some(42),
                action: MemberAuditAction::Ban,
                occurred_at_ms: 1_200,
                reason: None,
            }],
            (None, true),
            301_103,
            15_000,
        )
        .await
        .unwrap();
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 301_104)
            .await
            .unwrap()
            .is_none()
    );
    let claim = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 316_103, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            claim,
            vec![MemberAuditSnapshot {
                entry_id: 102,
                user_id: 98,
                executor_id: Some(42),
                action: MemberAuditAction::Unban,
                occurred_at_ms: 1_300,
                reason: None,
            }],
            (None, true),
            316_104,
            15_000,
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 98)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
    let history = store
        .member_history(scope.clone(), 98, None, 50)
        .await
        .unwrap();
    assert!(history.entries.iter().any(|e| e.kind == "ban"));
    assert!(history.entries.iter().any(|e| e.kind == "unban"));
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 316_105)
            .await
            .unwrap()
            .is_none()
    );
    let check = store
        .next_member_target_check(scope.clone(), 316_105)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 99);
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 99,
                revision: check.revision,
                kind: MemberVerifyKind::Member,
                present: false,
                member: None,
                ban: None,
            },
            316_106,
        )
        .await
        .unwrap();
    let candidate = store
        .next_member_cleanup_candidate(scope.clone(), 316_107)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.user_id, 99);
    store
        .settle_member_cleanup(scope.clone(), candidate, false, 316_108)
        .await
        .unwrap();
    assert!(store.get_member(scope.clone(), 99).await.unwrap().is_none());
    assert!(store.get_member(scope.clone(), 98).await.unwrap().is_some());
}

#[tokio::test]
async fn own_audit_stages_until_exact_link_then_replay_keeps_distinct_actions() {
    let (f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let app_case = CaseId::new();
    store
        .record_case(
            scope.clone(),
            app_case.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: app_source.clone(),
                action: CaseAction::Ban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 77,
                actor_id: Some(42),
                occurred_at_ms: 2_100,
                reason: Some("application reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 3_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 3_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let audit = |entry_id| MemberAuditSnapshot {
        entry_id,
        user_id: 77,
        executor_id: Some(42),
        action: MemberAuditAction::Ban,
        occurred_at_ms: 2_500,
        reason: Some("audit reason".into()),
    };
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            claim,
            vec![audit(501), audit(502)],
            (None, true),
            3_001,
            15_000,
        )
        .await
        .unwrap();
    let reopened = GuildDataStore::open(f.options(), store.expected.clone()).unwrap();
    assert!(
        reopened
            .case_for_source(scope.clone(), CaseSource::DiscordAudit { entry_id: 501 })
            .await
            .unwrap()
            .is_none()
    );
    let mut after = None;
    let mut observed = Vec::new();
    loop {
        let page = reopened
            .member_history(scope.clone(), 77, after, 1)
            .await
            .unwrap();
        observed.extend(page.entries);
        if page.next_cursor.is_none() {
            break;
        }
        after = page.next_cursor;
    }
    assert_eq!(observed.len(), 3);
    for entry_id in ["501", "502"] {
        let entry = observed
            .iter()
            .find(|row| row.observation_id.as_deref() == Some(entry_id))
            .unwrap();
        assert_eq!(entry.kind, "ban");
        assert_eq!(entry.occurred_at_ms, Some(2_500));
        assert_eq!(
            entry.source_kind.as_deref(),
            Some("discord_audit_unresolved")
        );
        assert_eq!(entry.case_id, None);
        assert_eq!(entry.outcome.as_deref(), Some("succeeded"));
        assert_eq!(entry.reason.as_deref(), Some("audit reason"));
    }

    reopened
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "CREATE TRIGGER refuse_unresolved_delete BEFORE DELETE ON guild_member_unresolved_audits BEGIN SELECT RAISE(ABORT,'test rollback'); END",
                SCHEMA_BATCH_LIMIT,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        reopened
            .link_case_source(
                scope.clone(),
                app_case.clone(),
                app_source.clone(),
                CaseSource::DiscordAudit { entry_id: 501 },
            )
            .await
            .is_err(),
        "failed unresolved consume must roll back the source association"
    );
    assert!(
        reopened
            .case_for_source(scope.clone(), CaseSource::DiscordAudit { entry_id: 501 })
            .await
            .unwrap()
            .is_none()
    );
    reopened
        .db
        .with_conn(|conn| {
            conn.execute_batch_bounded(
                "DROP TRIGGER refuse_unresolved_delete",
                SCHEMA_BATCH_LIMIT,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    reopened
        .link_case_source(
            scope.clone(),
            app_case.clone(),
            app_source.clone(),
            CaseSource::DiscordAudit { entry_id: 501 },
        )
        .await
        .unwrap();
    assert_eq!(
        reopened
            .case_for_source(scope.clone(), CaseSource::DiscordAudit { entry_id: 501 })
            .await
            .unwrap()
            .unwrap()
            .id,
        app_case
    );
    let linked = reopened
        .member_history(scope.clone(), 77, None, 50)
        .await
        .unwrap();
    assert_eq!(linked.entries.len(), 2);
    assert!(
        linked
            .entries
            .iter()
            .any(|row| row.observation_id.as_deref() == Some("502"))
    );
    let app_row = linked
        .entries
        .iter()
        .find(|row| row.case_id.is_some())
        .unwrap();
    assert_eq!(app_row.reason.as_deref(), Some("application reason"));
    assert_eq!(
        app_row.source_kind.as_deref(),
        Some("application_audit_linked")
    );

    let unban_stage = reopened
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 18_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    reopened
        .commit_member_audit_recovery_page(
            scope.clone(),
            unban_stage,
            vec![],
            (None, true),
            18_002,
            15_000,
        )
        .await
        .unwrap();
    reopened
        .mark_member_reconciliation_needed(scope.clone(), "another_gap", 20_000)
        .await
        .unwrap();
    let replay = reopened
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 20_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let revision_before: i64 = reopened
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT revision FROM guild_member_directory WHERE guild_id=10 AND user_id=77",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    reopened
        .commit_member_audit_recovery_page(
            scope.clone(),
            replay,
            vec![audit(501), audit(502)],
            (None, true),
            20_001,
            15_000,
        )
        .await
        .unwrap();
    let revision_after: i64 = reopened
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT revision FROM guild_member_directory WHERE guild_id=10 AND user_id=77",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        revision_after, revision_before,
        "replayed audit should not rewrite an already retained profile"
    );
    assert_eq!(
        reopened
            .member_history(scope.clone(), 77, None, 50)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    let second_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let second_case = CaseId::new();
    reopened
        .record_case(
            scope.clone(),
            second_case.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: second_source.clone(),
                action: CaseAction::Ban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 77,
                actor_id: Some(42),
                occurred_at_ms: 2_200,
                reason: Some("second action".into()),
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    reopened
        .link_case_source(
            scope.clone(),
            second_case.clone(),
            second_source,
            CaseSource::DiscordAudit { entry_id: 502 },
        )
        .await
        .unwrap();
    let distinct = reopened
        .member_history(scope.clone(), 77, None, 50)
        .await
        .unwrap();
    assert_eq!(distinct.entries.len(), 2);
    assert!(
        distinct
            .entries
            .iter()
            .any(|row| row.case_id.as_deref() == Some(app_case.as_str()))
    );
    assert!(
        distinct
            .entries
            .iter()
            .any(|row| row.case_id.as_deref() == Some(second_case.as_str()))
    );
    assert_ne!(app_case, second_case);
}

#[tokio::test]
async fn removed_application_case_consumes_exact_staged_audit_as_tombstone() {
    let (f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let case_id = CaseId::new();
    store
        .record_case(
            scope.clone(),
            case_id.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: app_source.clone(),
                action: CaseAction::Unban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 78,
                actor_id: Some(42),
                occurred_at_ms: 2_100,
                reason: Some("private application reason".into()),
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 3_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 3_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            claim,
            vec![MemberAuditSnapshot {
                entry_id: 503,
                user_id: 78,
                executor_id: Some(42),
                action: MemberAuditAction::Unban,
                occurred_at_ms: 2_500,
                reason: Some("audit reason".into()),
            }],
            (None, true),
            3_001,
            15_000,
        )
        .await
        .unwrap();
    assert!(
        store
            .remove_case(scope.clone(), case_id.clone())
            .await
            .unwrap()
    );
    let reopened = GuildDataStore::open(f.options(), store.expected.clone()).unwrap();
    assert!(
        reopened
            .member_history(scope.clone(), 78, None, 50)
            .await
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.observation_id.as_deref() == Some("503"))
    );
    reopened
        .link_case_source(
            scope.clone(),
            case_id.clone(),
            app_source,
            CaseSource::DiscordAudit { entry_id: 503 },
        )
        .await
        .unwrap();
    let replay = reopened
        .case_for_source(scope.clone(), CaseSource::DiscordAudit { entry_id: 503 })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.id, case_id);
    assert!(replay.removed);
    assert!(
        reopened
            .member_history(scope.clone(), 78, None, 50)
            .await
            .unwrap()
            .entries
            .is_empty(),
        "exact late link leaves only the removed-case replay key"
    );
}

#[tokio::test]
async fn version_eleven_upgrade_keeps_completed_seed_and_reconcile_cursor() {
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
        SNAPSHOT_SCHEMA,
        MEMBER_SCHEMA,
        MEMBER_STATUS_SCHEMA,
        MEMBER_PROGRESS_SCHEMA,
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
        "INSERT INTO guild_memberships VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',100,1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb');
        UPDATE guild_member_feature SET seed_admitted=1 WHERE guild_id=10;
        INSERT INTO guild_member_seed(guild_id,enrollment_id,bot_id,initialized_at_ms,initial_completed_at_ms,case_backfill_done) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,1000,2000,1);
        INSERT INTO guild_member_sync(guild_id,enrollment_id,bot_id,run_id,run_kind,phase,status,cursor,started_at_ms,member_processed,ban_processed,verify_processed,audit_processed) VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',42,'cccccccccccccccccccccccccccccccc','reconcile','members','queued','123',2100,7,2,1,3);",
        SCHEMA_BATCH_LIMIT,
    )
    .unwrap();
    raw.pragma_update(None, "user_version", 12).unwrap();
    drop(raw);
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
    let status = store
        .initialize_member_directory(scope.clone(), 42, 3_000)
        .await
        .unwrap();
    assert_eq!(status.initial_completed_at, Some(2_000));
    assert_eq!(
        status.run_id.as_deref(),
        Some("cccccccccccccccccccccccccccccccc")
    );
    assert_eq!(status.member_processed, Some(7));
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 3_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.cursor.as_deref(), Some("123"));
    let version: i64 = store
        .db
        .with_conn(|conn| conn.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?)))
        .await
        .unwrap();
    assert_eq!(version, VERSION);
    let unresolved_table: i64 = store
        .db
        .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='guild_member_unresolved_audits'", (), |r| Ok(r.get(0)?)))
        .await
        .unwrap();
    assert_eq!(unresolved_table, 1);
}

#[tokio::test]
async fn gateway_ban_identity_updates_saved_names_without_losing_nickname_or_order() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let query = |q: &str, status| MemberQuery {
        q: Some(q.into()),
        status,
        joined_from_ms: None,
        joined_to_ms: None,
        sort: MemberSort::JoinedNewest,
        after: None,
        limit: 10,
    };

    // The user was absent from the seed. A later Gateway ban supplies real
    // identity even though the directory has no prior member row to enrich.
    store
        .observe_ban_with_identity(
            scope.clone(),
            501,
            2_000,
            "BanUser".into(),
            Some("Ban Display".into()),
        )
        .await
        .unwrap();
    let banned = store.get_member(scope.clone(), 501).await.unwrap().unwrap();
    assert_eq!(banned.username.as_deref(), Some("BanUser"));
    assert_eq!(banned.global_name.as_deref(), Some("Ban Display"));
    assert!(banned.is_banned && banned.ever_banned);
    for name in ["banuser", "ban display", "501"] {
        assert_eq!(
            store
                .list_members(scope.clone(), query(name, MemberStatusFilter::Banned))
                .await
                .unwrap()
                .members
                .len(),
            1,
            "ban identity must be searchable by name and ID"
        );
    }
    store
        .observe_unban_with_identity(scope.clone(), 501, 3_000, "UnbanUser".into(), None)
        .await
        .unwrap();
    let unbanned = store.get_member(scope.clone(), 501).await.unwrap().unwrap();
    assert_eq!(unbanned.username.as_deref(), Some("UnbanUser"));
    assert_eq!(unbanned.global_name, None);
    assert!(!unbanned.is_banned && unbanned.ever_banned);
    assert_eq!(
        store
            .list_members(
                scope.clone(),
                query("unbanuser", MemberStatusFilter::PreviouslyBanned)
            )
            .await
            .unwrap()
            .members
            .len(),
        1
    );
    assert!(
        store
            .list_members(scope.clone(), query("ban display", MemberStatusFilter::All))
            .await
            .unwrap()
            .members
            .is_empty()
    );

    store
        .observe_member(
            scope.clone(),
            MemberObservation {
                user_id: 502,
                username: Some(Some("MemberUser".into())),
                global_name: Some(Some("Member Display".into())),
                nickname: Some(Some("Favorite Nick".into())),
                joined_at_ms: Some(1_200),
                observed_at_ms: 1_500,
                is_join: true,
            },
        )
        .await
        .unwrap();
    store
        .observe_ban_with_identity(
            scope.clone(),
            502,
            2_000,
            "Current Ban".into(),
            Some("Current Global".into()),
        )
        .await
        .unwrap();
    let known = store.get_member(scope.clone(), 502).await.unwrap().unwrap();
    assert_eq!(known.nickname.as_deref(), Some("Favorite Nick"));
    assert_eq!(known.username.as_deref(), Some("Current Ban"));
    assert_eq!(known.global_name.as_deref(), Some("Current Global"));
    assert_eq!(
        store
            .list_members(
                scope.clone(),
                query("favorite nick", MemberStatusFilter::Banned)
            )
            .await
            .unwrap()
            .members
            .len(),
        1
    );
    // An older opposite event must be verified and must not overwrite names.
    store
        .observe_unban_with_identity(scope.clone(), 502, 1_900, "Stale Name".into(), None)
        .await
        .unwrap();
    let after_stale = store.get_member(scope.clone(), 502).await.unwrap().unwrap();
    assert!(after_stale.is_banned);
    assert_eq!(after_stale.username.as_deref(), Some("Current Ban"));
    assert_eq!(after_stale.global_name.as_deref(), Some("Current Global"));
    store
        .observe_ban_with_identity(scope.clone(), 502, 2_100, "Fresh Ban".into(), None)
        .await
        .unwrap();
    let refreshed = store.get_member(scope.clone(), 502).await.unwrap().unwrap();
    assert_eq!(refreshed.username.as_deref(), Some("Fresh Ban"));
    assert_eq!(refreshed.global_name, None);
    assert_eq!(refreshed.nickname.as_deref(), Some("Favorite Nick"));
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

#[tokio::test]
async fn audit_alias_before_recovery_retains_a_confirmed_unban() {
    let (_f, store, scope) = ready().await;
    departed_member(&store, &scope, 61).await;
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let case_id = CaseId::new();
    store
        .record_case(
            scope.clone(),
            case_id.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: app_source.clone(),
                action: CaseAction::Unban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 61,
                actor_id: Some(42),
                occurred_at_ms: 2_600,
                reason: Some("command unban".into()),
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    assert!(
        !store
            .get_member(scope.clone(), 61)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
    store
        .link_case_source(
            scope.clone(),
            case_id,
            app_source,
            CaseSource::DiscordAudit { entry_id: 9001 },
        )
        .await
        .unwrap();
    let retained = store.get_member(scope.clone(), 61).await.unwrap().unwrap();
    assert!(retained.ever_banned);
    assert!(!retained.is_banned);
    assert!(
        store
            .backfill_member_case_index(scope.clone(), 42, 100)
            .await
            .unwrap()
    );
    assert!(
        store
            .next_member_cleanup_candidate(scope.clone(), 400_000)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn recovery_before_alias_and_attempted_unban_keep_retention_rules() {
    let (_f, store, scope) = ready().await;
    departed_member(&store, &scope, 62).await;
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 3_000)
        .await
        .unwrap();
    let claim = store
        .claim_member_audit_recovery(scope.clone(), 42, "worker".into(), 3_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    store
        .commit_member_audit_recovery_page(
            scope.clone(),
            claim,
            vec![MemberAuditSnapshot {
                entry_id: 9002,
                user_id: 62,
                executor_id: Some(42),
                action: MemberAuditAction::Unban,
                occurred_at_ms: 2_700,
                reason: None,
            }],
            (None, true),
            3_001,
            15_000,
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 62)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let case_id = CaseId::new();
    store
        .record_case(
            scope.clone(),
            case_id.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: app_source.clone(),
                action: CaseAction::Unban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 62,
                actor_id: Some(42),
                occurred_at_ms: 2_700,
                reason: None,
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .link_case_source(
            scope.clone(),
            case_id,
            app_source,
            CaseSource::DiscordAudit { entry_id: 9002 },
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope.clone(), 62)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );

    departed_member(&store, &scope, 63).await;
    let noop_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let noop_case = CaseId::new();
    store
        .record_case(
            scope.clone(),
            noop_case.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: noop_source.clone(),
                action: CaseAction::Unban,
                outcome: CaseOutcome::Attempted,
                subject_id: 63,
                actor_id: Some(42),
                occurred_at_ms: 2_800,
                reason: Some("already unbanned".into()),
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .link_case_source(
            scope.clone(),
            noop_case,
            noop_source,
            CaseSource::DiscordAudit { entry_id: 9003 },
        )
        .await
        .unwrap();
    assert!(
        !store
            .get_member(scope.clone(), 63)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
}

#[tokio::test]
async fn newer_local_ban_after_unban_still_requests_a_polarity_check() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store.observe_unban(scope.clone(), 64, 4_000).await.unwrap();
    store
        .observe_ban_contested(scope.clone(), 64, 5_000, "User".into(), None)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 64).await.unwrap().unwrap();
    assert!(member.is_banned);
    let check = store
        .next_member_target_check(scope.clone(), 5_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.reason, "ban_order_uncertain");
    assert_eq!(check.kind, MemberVerifyKind::Ban);
    store
        .settle_member_target_check(
            scope.clone(),
            check.clone(),
            MemberVerification {
                user_id: 64,
                revision: check.revision,
                kind: MemberVerifyKind::Ban,
                present: true,
                member: None,
                ban: Some(MemberBanSnapshot {
                    user_id: 64,
                    username: None,
                    global_name: None,
                    reason: None,
                }),
            },
            5_100,
        )
        .await
        .unwrap();
    store.observe_ban(scope.clone(), 65, 6_000).await.unwrap();
    store
        .observe_unban_contested(scope.clone(), 65, 7_000, "User".into(), None)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 65).await.unwrap().unwrap();
    assert!(!member.is_banned);
    let check = store
        .next_member_target_check(scope.clone(), 7_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 65);
    assert_eq!(check.reason, "ban_order_uncertain");
}

#[tokio::test]
async fn contested_repeat_of_the_current_polarity_still_requests_a_check() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store.observe_unban(scope.clone(), 70, 4_000).await.unwrap();
    store
        .observe_unban_contested(scope.clone(), 70, 5_000, "User".into(), None)
        .await
        .unwrap();
    let member = store.get_member(scope.clone(), 70).await.unwrap().unwrap();
    assert!(!member.is_banned);
    let check = store
        .next_member_target_check(scope.clone(), 5_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.user_id, 70);
    assert_eq!(check.reason, "ban_order_uncertain");
}

#[tokio::test]
async fn linking_an_audit_that_is_already_attached_still_retains_the_unban() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    departed_member(&store, &scope, 71).await;
    let app_source = CaseSource::Application {
        operation_id: CaseId::new(),
    };
    let case_id = CaseId::new();
    store
        .record_case(
            scope.clone(),
            case_id.clone(),
            CaseData {
                ban_attempt_tag: None,
                source: app_source.clone(),
                action: CaseAction::Unban,
                outcome: CaseOutcome::Succeeded,
                subject_id: 71,
                actor_id: Some(42),
                occurred_at_ms: 2_500,
                reason: None,
                evidence: CaseEvidence::Unavailable,
            },
            CaseLimits::default(),
        )
        .await
        .unwrap();
    store
        .link_case_source(
            scope.clone(),
            case_id.clone(),
            app_source.clone(),
            CaseSource::DiscordAudit { entry_id: 9004 },
        )
        .await
        .unwrap();
    store
        .db
        .with_conn(|conn| {
            let mut tx = conn.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.member_execute(
                "UPDATE guild_member_directory SET ever_banned=0 WHERE guild_id=10 AND user_id=71",
                (),
            )?;
            tx.commit()
        })
        .await
        .unwrap();
    store
        .link_case_source(
            scope.clone(),
            case_id,
            app_source,
            CaseSource::DiscordAudit { entry_id: 9004 },
        )
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope, 71)
            .await
            .unwrap()
            .unwrap()
            .ever_banned
    );
}

#[tokio::test]
async fn status_counts_due_absence_rows_instead_of_the_unused_sync_column() {
    let (_f, store, scope) = ready().await;
    departed_member(&store, &scope, 88).await;
    let due = store
        .member_sync_status(scope.clone(), 42, 400_000)
        .await
        .unwrap();
    assert_eq!(due.unseen_candidate_count, 1);
    let waiting = store.member_sync_status(scope, 42, 2_500).await.unwrap();
    assert_eq!(waiting.unseen_candidate_count, 0);
}

#[tokio::test]
async fn worker_status_skips_the_absence_backlog_count() {
    let (_f, store, scope) = ready().await;
    departed_member(&store, &scope, 89).await;
    let dashboard = store
        .member_sync_status(scope.clone(), 42, 400_000)
        .await
        .unwrap();
    let worker = store
        .member_sync_worker_status(scope, 42, 400_000)
        .await
        .unwrap();
    assert_eq!(dashboard.unseen_candidate_count, 1);
    assert_eq!(worker.unseen_candidate_count, 0);
    assert_eq!(worker.status, dashboard.status);
    assert_eq!(worker.next_attempt_at, dashboard.next_attempt_at);
}

#[tokio::test]
async fn newer_unchanged_member_observations_fence_delayed_names() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let mut current = observed(81, Some(500), 1_000);
    current.username = Some(Some("Ada".into()));
    store
        .observe_member(scope.clone(), current.clone())
        .await
        .unwrap();

    let mut unchanged = current.clone();
    unchanged.is_join = false;
    unchanged.observed_at_ms = 2_000;
    store
        .observe_member(scope.clone(), unchanged)
        .await
        .unwrap();
    let mut delayed = current.clone();
    delayed.is_join = false;
    delayed.observed_at_ms = 1_500;
    delayed.username = Some(Some("Older".into()));
    store
        .observe_member(scope.clone(), delayed.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .get_member(scope.clone(), 81)
            .await
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("Ada")
    );

    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 2_000, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut roster = page(MemberSyncPhase::Members, None, true);
    current.observed_at_ms = 3_000;
    roster.members = vec![current];
    store
        .commit_member_page(scope.clone(), claim, roster, 3_001)
        .await
        .unwrap();
    delayed.observed_at_ms = 2_500;
    store.observe_member(scope.clone(), delayed).await.unwrap();
    assert_eq!(
        store
            .get_member(scope, 81)
            .await
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("Ada")
    );
}

#[tokio::test]
async fn unchanged_ban_page_fences_delayed_unban() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_ban_with_identity(scope.clone(), 82, 1_000, "Ada".into(), None)
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
    bans.bans.push(MemberBanSnapshot {
        user_id: 82,
        username: Some("Ada".into()),
        global_name: None,
        reason: None,
    });
    store
        .commit_member_page(scope.clone(), claim, bans, 16_002)
        .await
        .unwrap();
    store
        .observe_unban(scope.clone(), 82, 15_000)
        .await
        .unwrap();
    assert!(
        store
            .get_member(scope, 82)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
}

#[tokio::test]
async fn ban_page_claim_is_discarded_when_unban_commits_during_fetch() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_ban_with_identity(scope.clone(), 86, 1_000, "Ada".into(), None)
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
    store
        .observe_unban(scope.clone(), 86, 16_002)
        .await
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, None, true);
    bans.bans.push(MemberBanSnapshot {
        user_id: 86,
        username: Some("Ada".into()),
        global_name: None,
        reason: None,
    });
    assert!(
        store
            .commit_member_page(scope.clone(), claim, bans, 16_003)
            .await
            .is_err()
    );
    assert!(
        !store
            .get_member(scope, 86)
            .await
            .unwrap()
            .unwrap()
            .is_banned
    );
}

#[tokio::test]
async fn import_progress_accepts_more_than_one_thousand_ready_guilds() {
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
                member_count: 1,
                page_size: 100,
                pace_ms: 15_000,
                active_guild_count: 1_001,
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
    let mut first = page(MemberSyncPhase::Members, None, true);
    first.members = vec![observed(83, Some(500), 1_001)];
    first.active_guild_count = 1_001;
    let status = store
        .commit_member_page(scope, claim, first, 1_002)
        .await
        .unwrap();
    assert_eq!(status.processed, 1);
    assert_eq!(status.phase.as_deref(), Some("bans"));
}

#[tokio::test]
async fn replaced_target_check_rejects_old_result_and_failure_at_the_same_millisecond() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .observe_member(scope.clone(), observed(84, Some(500), 1_000))
        .await
        .unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            84,
            MemberVerifyKind::Member,
            "coverage_uncertain",
            2_000,
        )
        .await
        .unwrap();
    let old = store
        .next_member_target_check(scope.clone(), 2_000)
        .await
        .unwrap()
        .unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            84,
            MemberVerifyKind::Member,
            "coverage_uncertain",
            2_000,
        )
        .await
        .unwrap();
    let newer = store
        .next_member_target_check(scope.clone(), 2_000)
        .await
        .unwrap()
        .unwrap();
    assert!(newer.requested_at_ms > old.requested_at_ms);
    assert!(
        store
            .defer_member_target_check(scope.clone(), old.clone(), 10_000)
            .await
            .is_err()
    );
    assert!(
        store
            .settle_member_target_check(
                scope.clone(),
                old.clone(),
                MemberVerification {
                    user_id: 84,
                    revision: old.revision,
                    kind: MemberVerifyKind::Member,
                    present: false,
                    member: None,
                    ban: None
                },
                2_100
            )
            .await
            .is_err()
    );
    assert!(
        store
            .get_member(scope.clone(), 84)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
    assert_eq!(
        store
            .next_member_target_check(scope, 2_100)
            .await
            .unwrap()
            .unwrap()
            .requested_at_ms,
        newer.requested_at_ms
    );
}

#[tokio::test]
async fn stale_check_refresh_only_requeues_the_same_stored_work_item() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let original = observed(85, Some(500), 1_000);
    store
        .observe_member(scope.clone(), original.clone())
        .await
        .unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            85,
            MemberVerifyKind::Member,
            "old_reason",
            1_100,
        )
        .await
        .unwrap();
    let old = store
        .next_member_target_check(scope.clone(), 1_100)
        .await
        .unwrap()
        .unwrap();
    store
        .queue_member_target_check(
            scope.clone(),
            85,
            MemberVerifyKind::Member,
            "new_reason",
            1_200,
        )
        .await
        .unwrap();
    assert!(
        !store
            .refresh_stale_member_target_check(scope.clone(), old, 1_300)
            .await
            .unwrap()
    );
    let current = store
        .next_member_target_check(scope.clone(), 1_300)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.reason, "new_reason");

    let mut unchanged = original;
    unchanged.is_join = false;
    unchanged.observed_at_ms = 2_000;
    store
        .observe_member(scope.clone(), unchanged)
        .await
        .unwrap();
    assert!(
        store
            .settle_member_target_check(
                scope.clone(),
                current.clone(),
                MemberVerification {
                    user_id: 85,
                    revision: current.revision,
                    kind: MemberVerifyKind::Member,
                    present: false,
                    member: None,
                    ban: None,
                },
                2_100,
            )
            .await
            .is_err()
    );
    assert!(
        store
            .refresh_stale_member_target_check(scope.clone(), current.clone(), 2_100)
            .await
            .unwrap()
    );
    let refreshed = store
        .next_member_target_check(scope.clone(), 2_100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.reason, "new_reason");
    assert!(refreshed.revision > current.revision);
    assert!(
        store
            .get_member(scope, 85)
            .await
            .unwrap()
            .unwrap()
            .is_current
    );
}

#[tokio::test]
async fn positive_update_after_departure_queues_current_revision() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    let original = observed(87, Some(500), 1_000);
    store
        .observe_member(scope.clone(), original.clone())
        .await
        .unwrap();
    store.depart_member(scope.clone(), 87, 2_000).await.unwrap();
    let mut update = original;
    update.is_join = false;
    update.observed_at_ms = 3_000;
    store.observe_member(scope.clone(), update).await.unwrap();
    let check = store
        .next_member_target_check(scope.clone(), 3_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(check.reason, "positive_update_join_uncertain");
    assert_eq!(
        check.revision,
        store.get_member(scope, 87).await.unwrap().unwrap().revision
    );
}

#[tokio::test]
async fn configured_large_roster_and_ban_pages_commit_with_one_cursor_each() {
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
    let mut members = page(MemberSyncPhase::Members, None, false);
    members.members = (1_000..1_500)
        .map(|id| observed(id, Some(500), 1_000))
        .collect();
    members.next_cursor = Some("1499".into());
    members.page_size = 500;
    let status = store
        .commit_member_page(scope.clone(), claim, members, 1_001)
        .await
        .unwrap();
    assert_eq!(status.processed, 500);
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 16_001, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut terminal = page(MemberSyncPhase::Members, claim.cursor.clone(), true);
    terminal.page_size = 500;
    store
        .commit_member_page(scope.clone(), claim, terminal, 16_002)
        .await
        .unwrap();
    let claim = store
        .claim_member_sync(scope.clone(), 42, "worker".into(), 31_002, 90_000)
        .await
        .unwrap()
        .unwrap();
    let mut bans = page(MemberSyncPhase::Bans, None, false);
    bans.bans = (2_000..2_500)
        .map(|id| MemberBanSnapshot {
            user_id: id,
            username: Some("Ada".into()),
            global_name: None,
            reason: Some("reason".into()),
        })
        .collect();
    bans.next_cursor = Some("2499".into());
    bans.page_size = 500;
    let status = store
        .commit_member_page(scope, claim, bans, 31_003)
        .await
        .unwrap();
    assert_eq!(status.ban_processed, Some(500));
}

const QUERY_COUNT_GUILD: i64 = 424_242;
const MEMBER_PAGE_CAP: usize = 500;
/// Lookup, set-based writes, revision bump, and the last-seen update.
const STATEMENTS_PER_MEMBER_BATCH: u64 = 14;
/// Claim seed read plus status() after the page, with a small fixed margin.
const STATEMENTS_PER_MEMBER_PAGE: u64 = 8;

fn member_batches(members: u64) -> u64 {
    let batch =
        u64::try_from(super::super::member_directory::MEMBER_PAGE_WRITE_BATCH_SIZE).unwrap();
    members.div_ceil(batch)
}

fn member_pages(members: u64) -> u64 {
    members.div_ceil(u64::try_from(MEMBER_PAGE_CAP).unwrap())
}

fn member_statement_bound(members: u64) -> u64 {
    STATEMENTS_PER_MEMBER_BATCH * member_batches(members)
        + STATEMENTS_PER_MEMBER_PAGE * member_pages(members)
}

async fn ready_guild(guild_id: i64) -> (Fixture, GuildDataStore, GuildDataScope) {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let placement = f.placement(&reg, guild_id).await;
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let scope = GuildDataScope::new(
        placement,
        crate::db::GuildEnrollment {
            guild_id,
            id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    store.bind_initial_membership(scope.clone()).await.unwrap();
    (f, store, scope)
}

async fn commit_roster(
    store: &GuildDataStore,
    scope: &GuildDataScope,
    members: &[MemberObservation],
    mut now_ms: i64,
) -> (u64, i64, MemberSyncStatus) {
    begin_member_sync_statement_count(scope.guild_id());
    let mut status = None;
    for chunk in members.chunks(MEMBER_PAGE_CAP) {
        let claim = store
            .claim_member_sync(scope.clone(), 42, "worker".into(), now_ms, 90_000)
            .await
            .unwrap()
            .expect("member page is due");
        assert_eq!(claim.phase, MemberSyncPhase::Members);
        let mut page = page(MemberSyncPhase::Members, claim.cursor.clone(), false);
        page.members = chunk.to_vec();
        page.next_cursor = Some(format!("{}", chunk.last().unwrap().user_id));
        page.page_size = u16::try_from(MEMBER_PAGE_CAP).unwrap();
        page.total_estimate = Some(i64::try_from(members.len()).unwrap());
        page.pace_ms = 15_000;
        status = Some(
            store
                .commit_member_page(scope.clone(), claim, page, now_ms)
                .await
                .unwrap(),
        );
        now_ms = now_ms.saturating_add(20_000);
    }
    (
        member_sync_statement_count(scope.guild_id()),
        now_ms,
        status.unwrap(),
    )
}

fn assert_member_statement_bound(kind: &str, members: u64, queries: u64) {
    let bound = member_statement_bound(members);
    assert!(
        queries > 0,
        "{kind} for {members} members counted no statements"
    );
    let batch =
        u64::try_from(super::super::member_directory::MEMBER_PAGE_WRITE_BATCH_SIZE).unwrap();
    assert!(
        queries < members || members <= batch,
        "{kind} for {members} members issued {queries} statements"
    );
    assert!(
        queries <= bound,
        "{kind} for {members} members issued {queries} statements, above batch bound {bound}"
    );
}

async fn ready_with_probe(
    probe: &std::sync::Arc<crate::db_compat::StartupProbe>,
) -> (Fixture, GuildDataStore, GuildDataScope) {
    let _guard = crate::db_compat::enter_startup_probe(Some(probe.clone()));
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
async fn coverage_gap_mark_batches_writes_for_remote_transport() {
    let probe = std::sync::Arc::new(crate::db_compat::StartupProbe::default());
    let (_f, mut store, scope) = ready_with_probe(&probe).await;
    complete_for_checks(&store, &scope).await;
    crate::db::test_mark_connection_pool_remote(&mut store.db);
    let queries = probe.queries();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 122_000)
        .await
        .unwrap();
    let used = probe.queries() - queries;
    assert_eq!(
        used, 12,
        "coverage-gap mark remote round-trips (scope read, write tx, receipt, batch, post-check, commit)"
    );
    let status = store
        .member_sync_status(scope.clone(), 42, 122_001)
        .await
        .unwrap();
    assert!(status.reconciliation_needed);
    assert_eq!(status.reconciliation_reason.as_deref(), Some("gateway_gap"));
}

#[tokio::test]
async fn coverage_gap_mark_skips_audit_recovery_until_initial_completion() {
    let (_f, store, scope) = ready().await;
    store
        .initialize_member_directory(scope.clone(), 42, 1_000)
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 3_000)
        .await
        .unwrap();
    let audit: Option<i64> = store
        .db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let row = tx
                .query_row(
                    "SELECT bot_id FROM guild_member_audit_recovery WHERE guild_id=10",
                    (),
                    |r| Ok(r.get(0)?),
                )
                .optional()?;
            tx.finish_read()?;
            Ok(row)
        })
        .await
        .unwrap();
    assert!(audit.is_none());
    let status = store
        .member_sync_status(scope.clone(), 42, 3_001)
        .await
        .unwrap();
    assert!(status.reconciliation_needed);
}

#[tokio::test]
async fn coverage_gap_mark_rejects_closed_membership() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store.close_membership(scope.clone()).await.unwrap();
    let error = store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 122_000)
        .await
        .unwrap_err();
    assert!(error.is::<TenantDataError>());
    let audit: Option<i64> = store
        .db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let row = tx
                .query_row(
                    "SELECT bot_id FROM guild_member_audit_recovery WHERE guild_id=10",
                    (),
                    |r| Ok(r.get(0)?),
                )
                .optional()?;
            tx.finish_read()?;
            Ok(row)
        })
        .await
        .unwrap();
    assert!(audit.is_none());
}

#[tokio::test]
async fn coverage_gap_mark_rejects_membership_closed_after_scope_check() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    let store_for_close = store.clone();
    let scope_for_close = scope.clone();
    super::super::member_directory::test_set_coverage_gap_after_scope_verify(Some(Box::new(
        move || {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle
                    .block_on(store_for_close.close_membership(scope_for_close.clone()))
                    .unwrap();
            }
        },
    )));
    let error = store
        .mark_member_reconciliation_needed(scope.clone(), "gateway_gap", 122_000)
        .await
        .unwrap_err();
    super::super::member_directory::test_set_coverage_gap_after_scope_verify(None);
    assert!(error.is::<TenantDataError>());
    let reconciliation_needed: bool = store
        .db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let row = tx.query_row(
                "SELECT reconciliation_needed FROM guild_member_seed WHERE guild_id=10",
                (),
                |r| Ok(r.get::<bool>(0)?),
            )?;
            tx.finish_read()?;
            Ok(row)
        })
        .await
        .unwrap();
    assert!(!reconciliation_needed);
}

#[tokio::test]
async fn coverage_gap_mark_rejects_nul_reason() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    let error = store
        .mark_member_reconciliation_needed(scope.clone(), "gap\0reason", 122_000)
        .await
        .unwrap_err();
    assert!(error.is::<MemberSyncError>());
}

#[tokio::test]
async fn coverage_gap_mark_is_idempotent_across_retries() {
    let (_f, store, scope) = ready().await;
    complete_for_checks(&store, &scope).await;
    store
        .mark_member_reconciliation_needed(scope.clone(), "first_gap", 3_000)
        .await
        .unwrap();
    store
        .mark_member_reconciliation_needed(scope.clone(), "second_gap", 4_000)
        .await
        .unwrap();
    let generation: i64 = store
        .db
        .with_conn(|conn| {
            let tx = conn.transaction_primary_snapshot()?;
            let row = tx.query_row(
                "SELECT gap_generation FROM guild_member_seed WHERE guild_id=10",
                (),
                |r| Ok(r.get(0)?),
            )?;
            tx.finish_read()?;
            Ok(row)
        })
        .await
        .unwrap();
    assert_eq!(generation, 2);
    let status = store
        .member_sync_status(scope.clone(), 42, 4_001)
        .await
        .unwrap();
    assert_eq!(status.reconciliation_reason.as_deref(), Some("second_gap"));
}

#[tokio::test]
async fn member_page_statements_scale_by_batch_not_by_member() {
    for members in [10_usize, 1_000] {
        let (_f, store, scope) = ready_guild(QUERY_COUNT_GUILD).await;
        store
            .initialize_member_directory(scope.clone(), 42, 1_000)
            .await
            .unwrap();
        let imported: Vec<_> = (1..=members as i64)
            .map(|user_id| observed(user_id, Some(500), 2_000))
            .collect();
        let (import_queries, now_ms, import_status) =
            commit_roster(&store, &scope, &imported, 2_000).await;
        assert_eq!(import_status.inserted, members as i64);
        assert_member_statement_bound("import", members as u64, import_queries);

        let refreshed: Vec<_> = (1..=members as i64)
            .map(|user_id| observed(user_id, Some(500), 100_000 + user_id))
            .collect();
        let (refresh_queries, _, refresh_status) =
            commit_roster(&store, &scope, &refreshed, now_ms).await;
        assert_eq!(refresh_status.unchanged, members as i64);
        assert_member_statement_bound("refresh", members as u64, refresh_queries);
        let first = store.get_member(scope.clone(), 1).await.unwrap().unwrap();
        let last = store
            .get_member(scope, members as i64)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.observed_at_ms, 100_001);
        assert_eq!(last.observed_at_ms, 100_000 + members as i64);
        assert_eq!(first.revision, 1);
        assert_eq!(last.revision, 1);
    }
}

#[tokio::test]
async fn cleanup_candidate_plan_uses_the_existing_scoped_index() {
    let (_fixture, store, scope) = ready().await;
    store
        .db
        .with_conn(move |conn| {
            crate::db::tests::assert_index_plan(
                conn,
                super::super::member_directory::MEMBER_CLEANUP_CANDIDATE_SQL,
                params![
                    scope.guild_id(),
                    scope.enrollment.id.as_str(),
                    1_000_000,
                    1,
                    0
                ],
                false,
            );
            Ok(())
        })
        .await
        .unwrap();
}
