use super::cases::data;
use super::tenant_logs::scope;
use super::*;

fn app() -> CaseSource {
    CaseSource::Application {
        operation_id: CaseId::new(),
    }
}
fn audit(entry_id: i64) -> CaseSource {
    CaseSource::DiscordAudit { entry_id }
}
fn facts(source: CaseSource) -> CaseData {
    CaseData { source, ..data(1) }
}
async fn record(store: &GuildDataStore, scope: &GuildDataScope, source: CaseSource) -> CaseId {
    let id = CaseId::new();
    assert_eq!(
        store
            .record_case(
                scope.clone(),
                id.clone(),
                facts(source),
                CaseLimits::default()
            )
            .await
            .unwrap(),
        RecordCase::Created(id.clone())
    );
    id
}
fn found(id: &CaseId, removed: bool) -> CaseSourceMatch {
    CaseSourceMatch {
        id: id.clone(),
        removed,
    }
}

#[tokio::test]
async fn sources_resolve_both_orders_without_replacing_original_facts() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    for (original, counterpart) in [(app(), audit(10)), (audit(20), app())] {
        let id = record(&store, &a, original.clone()).await;
        assert_eq!(
            store
                .link_case_source(a.clone(), id.clone(), original.clone(), counterpart.clone())
                .await
                .unwrap(),
            LinkCaseSource::Linked(found(&id, false))
        );
        let reopened = GuildDataStore::open(f.options(), reg.clone()).unwrap();
        for source in [original.clone(), counterpart.clone()] {
            assert_eq!(
                reopened.case_for_source(a.clone(), source).await.unwrap(),
                Some(found(&id, false))
            );
        }
        assert_eq!(
            store
                .case(a.clone(), id.clone(), CaseLimits::default())
                .await
                .unwrap(),
            Some(facts(original.clone()))
        );
        assert_eq!(
            store
                .record_case(
                    a.clone(),
                    CaseId::new(),
                    facts(original),
                    CaseLimits::default()
                )
                .await
                .unwrap(),
            RecordCase::Existing(id.clone())
        );
        // A second producer must resolve and append selected history, not submit
        // different original facts under its newly linked source.
        assert!(
            store
                .record_case(
                    a.clone(),
                    CaseId::new(),
                    facts(counterpart),
                    CaseLimits::default()
                )
                .await
                .is_err()
        );
    }
    assert!(store.case_for_source(a, audit(99)).await.unwrap().is_none());
}

#[tokio::test]
async fn concurrent_links_conflicts_and_retries_do_not_rewrite_or_add_receipts() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let original = app();
    let id = record(&store, &a, original.clone()).await;
    let (one, two) = tokio::join!(
        store.link_case_source(a.clone(), id.clone(), original.clone(), audit(1)),
        store.link_case_source(a.clone(), id.clone(), original.clone(), audit(1))
    );
    let results = [one.unwrap(), two.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, LinkCaseSource::Linked(_)))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, LinkCaseSource::Existing(_)))
            .count(),
        1
    );
    let other = record(&store, &a, audit(2)).await;
    for (target, anchor, source) in [
        (id.clone(), original.clone(), audit(2)), // another case already owns it
        (id.clone(), original.clone(), audit(3)), // this kind already occupied
        (id.clone(), original.clone(), app()),    // original kind cannot be replaced
        (other, original.clone(), app()),         // anchor identifies wrong parent
        (id.clone(), app(), audit(1)),            // even existing link needs its anchor
    ] {
        assert!(
            store
                .link_case_source(a.clone(), target, anchor, source)
                .await
                .is_err()
        );
    }
    store.db.with_conn(|c| {
        c.execute_batch("CREATE TRIGGER refuse_source BEFORE INSERT ON guild_case_sources BEGIN SELECT RAISE(ABORT,'unexpected write'); END; CREATE TRIGGER refuse_receipt BEFORE INSERT ON mutation_receipts BEGIN SELECT RAISE(ABORT,'unexpected receipt'); END")?;
        Ok(())
    }).await.unwrap();
    assert_eq!(
        store
            .link_case_source(a.clone(), id.clone(), audit(1), original.clone())
            .await
            .unwrap(),
        LinkCaseSource::Existing(found(&id, false))
    );
    assert_eq!(
        store
            .link_case_source(a.clone(), id.clone(), original.clone(), audit(1))
            .await
            .unwrap(),
        LinkCaseSource::Existing(found(&id, false))
    );
    assert_eq!(
        store.case_for_source(a.clone(), audit(1)).await.unwrap(),
        Some(found(&id, false))
    );
    assert_eq!(
        store
            .record_case(a, CaseId::new(), facts(original), CaseLimits::default())
            .await
            .unwrap(),
        RecordCase::Existing(id)
    );
}

#[tokio::test]
async fn late_link_and_removal_race_preserve_replay_suppression_without_content() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    for entry in [1, 2] {
        let original = app();
        let id = record(&store, &a, original.clone()).await;
        if entry == 1 {
            store.remove_case(a.clone(), id.clone()).await.unwrap();
            assert_eq!(
                store
                    .link_case_source(a.clone(), id.clone(), original.clone(), audit(entry))
                    .await
                    .unwrap(),
                LinkCaseSource::Linked(found(&id, true))
            );
        } else {
            let (link, remove) = tokio::join!(
                store.link_case_source(a.clone(), id.clone(), original.clone(), audit(entry)),
                store.remove_case(a.clone(), id.clone())
            );
            assert!(matches!(link.unwrap(), LinkCaseSource::Linked(_)));
            assert!(remove.unwrap());
        }
        for source in [original.clone(), audit(entry)] {
            assert_eq!(
                store
                    .case_for_source(a.clone(), source.clone())
                    .await
                    .unwrap(),
                Some(found(&id, true))
            );
            assert_eq!(
                store
                    .record_case(
                        a.clone(),
                        CaseId::new(),
                        facts(source),
                        CaseLimits::default()
                    )
                    .await
                    .unwrap(),
                RecordCase::Removed(id.clone())
            );
        }
        assert!(
            store
                .case(a.clone(), id.clone(), CaseLimits::default())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .link_case_source(a.clone(), id.clone(), original, audit(entry))
                .await
                .unwrap(),
            LinkCaseSource::Existing(found(&id, true))
        );
    }
    store
        .db
        .with_conn(|c| {
            let payloads: i64 = c.query_row(
                "SELECT COUNT(*) FROM guild_cases WHERE payload IS NOT NULL",
                (),
                |r| Ok(r.get(0)?),
            )?;
            assert_eq!(payloads, 0);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn competing_record_or_link_never_steals_source_or_merges_cases() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let original = app();
    let id = record(&store, &a, original.clone()).await;
    let other = CaseId::new();
    let (link, record) = tokio::join!(
        store.link_case_source(a.clone(), id.clone(), original, audit(1)),
        store.record_case(
            a.clone(),
            other.clone(),
            facts(audit(1)),
            CaseLimits::default()
        )
    );
    match (link, record) {
        (Ok(LinkCaseSource::Linked(_)), Err(_)) => assert_eq!(
            store.case_for_source(a.clone(), audit(1)).await.unwrap(),
            Some(found(&id, false))
        ),
        (Err(_), Ok(RecordCase::Created(created))) => assert_eq!(created, other),
        result => panic!("unexpected competing result: {result:?}"),
    }
    // A source match must not mask a conflicting requested case ID, even when removed.
    let unrelated = self::record(&store, &a, audit(2)).await;
    store
        .remove_case(a.clone(), unrelated.clone())
        .await
        .unwrap();
    assert!(
        store
            .record_case(a, unrelated, facts(audit(1)), CaseLimits::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn source_cleanup_is_atomic_owned_and_refuses_closed_or_wrong_scopes() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let b = scope(&f, &store, &reg, 20).await;
    let original = app();
    let id = record(&store, &a, original.clone()).await;
    let other = record(&store, &b, original.clone()).await;
    store
        .link_case_source(a.clone(), id.clone(), original.clone(), audit(1))
        .await
        .unwrap();
    assert!(
        store
            .case_for_source(b.clone(), audit(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .link_case_source(b.clone(), id.clone(), original.clone(), audit(1))
            .await
            .is_err()
    );
    let wrong = GuildDataScope::new(
        a.placement.clone(),
        GuildEnrollment {
            id: "cccccccccccccccccccccccccccccccc".into(),
            ..a.enrollment.clone()
        },
    )
    .unwrap();
    assert!(
        store
            .case_for_source(wrong.clone(), audit(999))
            .await
            .is_err()
    );
    assert!(
        store
            .link_case_source(wrong, CaseId::new(), original.clone(), audit(1))
            .await
            .is_err()
    );
    store.close_membership(a.clone()).await.unwrap();
    assert!(
        store
            .case_for_source(a.clone(), original.clone())
            .await
            .is_err()
    );
    assert!(
        store
            .link_case_source(a.clone(), id.clone(), original.clone(), audit(1))
            .await
            .is_err()
    );
    store.db.with_conn(|c| {
        c.execute_batch("CREATE TRIGGER refuse_source_delete BEFORE DELETE ON guild_case_sources BEGIN SELECT RAISE(ABORT,'retain'); END")?;
        Ok(())
    }).await.unwrap();
    assert!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .is_err()
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_cases WHERE guild_id=10",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                1
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_case_sources WHERE guild_id=10",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                2
            );
            c.execute_batch("DROP TRIGGER refuse_source_delete")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .purge_closed_cases(a.clone(), 1, CaseLimits::default())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .purge_closed_cases(a, 1, CaseLimits::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store.case_for_source(b, original).await.unwrap(),
        Some(found(&other, false))
    );
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_case_sources WHERE guild_id=10",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn raw_sql_guards_protect_sources_and_rollback_parent_insert() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let a = scope(&f, &store, &reg, 10).await;
    let id = record(&store, &a, audit(1)).await;
    let original = app();
    store
        .link_case_source(a.clone(), id, audit(1), original.clone())
        .await
        .unwrap();
    store.db.with_conn(|c| {
        c.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA recursive_triggers=OFF")?;
        for sql in [
            "UPDATE guild_case_sources SET case_id='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
            "DELETE FROM guild_case_sources",
            "INSERT OR REPLACE INTO guild_case_sources SELECT * FROM guild_case_sources",
            "INSERT INTO guild_case_sources SELECT 20,enrollment_id,case_id,source_key,source_kind FROM guild_case_sources",
            "INSERT INTO guild_case_sources SELECT guild_id,enrollment_id,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',source_key,source_kind FROM guild_case_sources",
            "INSERT INTO guild_case_sources SELECT guild_id,enrollment_id,case_id,'discord:2',0 FROM guild_case_sources LIMIT 1",
            "INSERT INTO guild_case_sources SELECT guild_id,enrollment_id,case_id,'discord:01',1 FROM guild_case_sources LIMIT 1",
            "INSERT INTO guild_cases SELECT guild_id,enrollment_id,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',s.source_key,'{}',0 FROM guild_case_sources s WHERE source_kind=0",
        ] { assert!(c.execute_batch(sql).is_err(), "accepted {sql}"); }
        assert_eq!(c.query_row("SELECT COUNT(*) FROM guild_cases", (), |r| Ok(r.get::<i64>(0)?))?, 1);
        c.execute_batch("CREATE TRIGGER refuse_source BEFORE INSERT ON guild_case_sources BEGIN SELECT RAISE(ABORT,'retain'); END")?;
        Ok(())
    }).await.unwrap();
    let uncommitted = CaseId::new();
    assert!(
        store
            .record_case(
                a.clone(),
                uncommitted.clone(),
                facts(audit(2)),
                CaseLimits::default()
            )
            .await
            .is_err()
    );
    assert!(
        store
            .case(a.clone(), uncommitted, CaseLimits::default())
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.case_for_source(a.clone(), audit(0)).await.is_err());
    assert!(
        store
            .link_case_source(a, CaseId::new(), original, audit(-1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn version_four_upgrade_indexes_live_removed_and_closed_cases() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    let placement = f.placement(&reg, 10).await;
    let ids = [CaseId::new(), CaseId::new(), CaseId::new()];
    {
        let raw = Connection::open(&f.store_path).unwrap();
        for schema in [SCHEMA, TENANT_SCHEMA, CASE_SCHEMA, HISTORY_SCHEMA] {
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
        for (n, id) in ids.iter().enumerate() {
            raw.execute(
                "INSERT INTO guild_cases VALUES(10,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,?2,?3,0)",
                params![
                    id.as_str(),
                    audit(n as i64 + 1).key().unwrap(),
                    serde_json::to_string(&facts(audit(n as i64 + 1))).unwrap()
                ],
            )
            .unwrap();
        }
        raw.execute(
            "UPDATE guild_cases SET deleted=1,payload=NULL WHERE case_id=?1",
            params![ids[1].as_str()],
        )
        .unwrap();
        raw.execute_batch("UPDATE guild_memberships SET active=0")
            .unwrap();
        raw.pragma_update(None, "user_version", 4).unwrap();
    }
    assert!(GuildDataStore::open(f.options(), reg.clone()).is_err());
    let foreign = f.registration(StoreId::new(), StorePolicy::Shared).await;
    assert!(GuildDataStore::initialize(f.options(), foreign).is_err());
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    store
        .db
        .with_conn(|c| {
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM guild_case_sources", (), |r| Ok(r
                    .get::<i64>(
                    0
                )?))?,
                3
            );
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM guild_cases WHERE payload IS NOT NULL",
                    (),
                    |r| Ok(r.get::<i64>(0)?)
                )?,
                2
            );
            assert_eq!(
                c.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))?,
                5
            );
            Ok(())
        })
        .await
        .unwrap();
    let a = GuildDataScope::new(
        placement,
        GuildEnrollment {
            guild_id: 10,
            id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    assert!(store.case_for_source(a.clone(), audit(1)).await.is_err());
    assert_eq!(
        store
            .purge_closed_cases(a, 3, CaseLimits::default())
            .await
            .unwrap(),
        3
    );
}
