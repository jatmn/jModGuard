use super::*;

fn work_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

async fn primary_matching<P>(store: &GuildDataStore, sql: &str, params: P)
where
    P: libsql::params::IntoParams + Send + 'static,
{
    let sql = sql.to_owned();
    store
        .db
        .with_conn(move |c| {
            let mut tx = c.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute_matching(&sql, params)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn try_primary_rows<P>(
    store: &GuildDataStore,
    sql: &str,
    params: P,
    rows: usize,
) -> Result<()>
where
    P: libsql::params::IntoParams + Send + 'static,
{
    let sql = sql.to_owned();
    store
        .db
        .with_conn(move |c| {
            let mut tx = c.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            tx.execute_rows(&sql, params, rows as u64)?;
            tx.commit()?;
            Ok(())
        })
        .await
}

async fn primary_reclaim<P>(store: &GuildDataStore, sql: &str, params: P)
where
    P: libsql::params::IntoParams + Send + 'static,
{
    let sql = sql.to_owned();
    store
        .db
        .with_conn(move |c| {
            let mut tx = c.transaction_primary_read()?;
            tx.prepare_reclaim()?;
            tx.execute_reclaim(&sql, params)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn try_primary_matching<P>(store: &GuildDataStore, sql: &str, params: P) -> Result<usize>
where
    P: libsql::params::IntoParams + Send + 'static,
{
    let sql = sql.to_owned();
    store
        .db
        .with_conn(move |c| {
            let mut tx = c.transaction_primary_read()?;
            tx.ensure_mutation_receipt()?;
            let changed = tx.execute_matching(&sql, params)?;
            tx.commit()?;
            Ok(changed)
        })
        .await
}

async fn bound_scope(
    f: &Fixture,
    reg: &crate::db::guild_stores::StoreRegistration,
    guild: i64,
) -> (GuildDataStore, GuildDataScope) {
    let store = GuildDataStore::initialize(f.options(), reg.clone()).unwrap();
    let placement = f.placement(reg, guild).await;
    store
        .bind_initial_placement(placement.clone())
        .await
        .unwrap();
    let scope = GuildDataScope::new(
        placement,
        crate::db::GuildEnrollment {
            guild_id: guild,
            id: format!("{:032x}", rand::random::<u128>()),
            joined_at_ns: 100,
        },
    )
    .unwrap();
    store.bind_initial_membership(scope.clone()).await.unwrap();
    (store, scope)
}

#[tokio::test]
async fn fresh_store_is_version_seventeen_with_scheduled_work_objects() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    GuildDataStore::initialize(f.options(), reg).unwrap();
    let raw = crate::db_compat::Connection::open(&f.store_path).unwrap();
    assert_eq!(
        raw.query_row("PRAGMA user_version", (), |r| Ok(r.get::<i64>(0)?))
            .unwrap(),
        VERSION
    );
    let table: bool = raw
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_scheduled_work')",
            (),
            |r| Ok(r.get(0)?),
        )
        .unwrap();
    assert!(table);
}

#[tokio::test]
async fn version_sixteen_historical_upgrades_to_seventeen() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    GuildDataStore::create_historical_store(f.options(), reg.clone(), 16).unwrap();
    let store = GuildDataStore::initialize(f.options(), reg).unwrap();
    store.db.with_conn(move |c| {
        let version: i64 = c.query_row("PRAGMA user_version", (), |r| Ok(r.get(0)?))?;
        assert_eq!(version, VERSION);
        let table: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_scheduled_work')",
            (),
            |r| Ok(r.get(0)?),
        )?;
        assert!(table);
        Ok(())
    }).await.unwrap();
}

#[tokio::test]
async fn version_sixteen_historical_store_has_no_scheduled_work_table() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    GuildDataStore::create_historical_store(f.options(), reg, 16).unwrap();
    let raw = crate::db_compat::Connection::open(&f.store_path).unwrap();
    let table: bool = raw
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guild_scheduled_work')",
            (),
            |r| Ok(r.get(0)?),
        )
        .unwrap();
    assert!(!table);
}

#[tokio::test]
async fn insert_refuses_inactive_membership_bad_initial_state_and_oversize_payload() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    store.close_membership(scope.clone()).await.unwrap();
    assert!(
        store
            .insert_softban_unban_pending(&scope, 55, "r", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0)
            .await
            .is_err()
    );
    let f2 = Fixture::new();
    let reg2 = f2
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f2, &reg2, 11).await;
    let now = chrono::Utc::now().timestamp_millis();
    let gid = scope.guild_id();
    let eid = scope.enrollment().id.clone();
    let eid1 = eid.clone();
    assert!(
        try_primary_rows(
            &store,
            "INSERT INTO guild_scheduled_work(
                    guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                    state,attempt,claim_until_ms,created_at_ms,updated_at_ms
                 ) VALUES(?1,?2,?3,'reminder',55,?4,'{}','claimed',1,?4,?4,?4)",
            params![gid, eid1.as_str(), work_id(), now],
            1,
        )
        .await
        .is_err()
    );
    let huge = "x".repeat(2001);
    let eid2 = eid.clone();
    assert!(
        try_primary_rows(
            &store,
            "INSERT INTO guild_scheduled_work(
                    guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                    state,attempt,claim_until_ms,created_at_ms,updated_at_ms
                 ) VALUES(?1,?2,?3,'reminder',55,?4,?5,'pending',0,NULL,?4,?4)",
            params![gid, eid2.as_str(), work_id(), now, huge],
            1,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn illegal_transitions_abort() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    let id = store
        .insert_softban_unban_pending(&scope, 55, "r", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let claimed = store
        .claim_scheduled_work_row(
            scope.guild_id(),
            scope.enrollment().id.as_str(),
            &id,
            now,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .finish_scheduled_work(&claimed, "done", None, now)
        .await
        .unwrap();
    let pending_id = id.clone();
    assert!(
        try_primary_matching(
            &store,
            "UPDATE guild_scheduled_work SET state='pending' WHERE work_id=?1",
            params![pending_id.as_str()],
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("scheduled work transition refused")
    );
    let id2 = store
        .insert_softban_unban_pending(&scope, 56, "r", "cccccccccccccccccccccccccccccccc", 0)
        .await
        .unwrap();
    for (sql, id) in [
        (
            "UPDATE guild_scheduled_work SET due_at_ms=0 WHERE work_id=?1",
            id2.clone(),
        ),
        (
            "UPDATE guild_scheduled_work SET kind='reminder' WHERE work_id=?1",
            id2.clone(),
        ),
        (
            "UPDATE guild_scheduled_work SET payload='{}' WHERE work_id=?1",
            id2.clone(),
        ),
    ] {
        assert!(
            try_primary_matching(&store, sql, params![id.as_str()])
                .await
                .unwrap_err()
                .to_string()
                .contains("scheduled work transition refused")
        );
    }
}

#[tokio::test]
async fn two_open_ban_expiry_abort_while_two_reminders_allowed() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    let gid = scope.guild_id();
    let eid = scope.enrollment().id.clone();
    let now = chrono::Utc::now().timestamp_millis();
    let eid1 = eid.clone();
    try_primary_rows(
        &store,
        "INSERT INTO guild_scheduled_work(
                guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                state,attempt,claim_until_ms,created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,'ban_expiry',55,?4,'{}','pending',0,NULL,?4,?4)",
        params![gid, eid1.as_str(), work_id(), now],
        1,
    )
    .await
    .unwrap();
    let eid2 = eid.clone();
    assert!(
        try_primary_rows(
            &store,
            "INSERT INTO guild_scheduled_work(
                    guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                    state,attempt,claim_until_ms,created_at_ms,updated_at_ms
                 ) VALUES(?1,?2,?3,'ban_expiry',55,?4,'{}','pending',0,NULL,?4,?4)",
            params![gid, eid2.as_str(), work_id(), now],
            1,
        )
        .await
        .is_err()
    );
    for eid_part in [eid.clone(), eid.clone()] {
        try_primary_rows(
            &store,
            "INSERT INTO guild_scheduled_work(
                guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                state,attempt,claim_until_ms,created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,'reminder',55,?4,'{}','pending',0,NULL,?4,?4)",
            params![gid, eid_part.as_str(), work_id(), now],
            1,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn timezone_payload_replace_and_second_pending_timezone_aborts() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    let gid = scope.guild_id();
    let eid = scope.enrollment().id.clone();
    let now = chrono::Utc::now().timestamp_millis();
    let id = work_id();
    let eid1 = eid.clone();
    let row_id = id.clone();
    try_primary_rows(
        &store,
        "INSERT INTO guild_scheduled_work(
                guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                state,attempt,claim_until_ms,created_at_ms,updated_at_ms
             ) VALUES(?1,?2,?3,'user_timezone',55,?4,'{\"tz\":\"a\"}','pending',0,NULL,?4,?4)",
        params![gid, eid1.as_str(), row_id.as_str(), now],
        1,
    )
    .await
    .unwrap();
    let update_id = id.clone();
    primary_matching(
        &store,
        "UPDATE guild_scheduled_work SET payload='{\"tz\":\"b\"}' WHERE work_id=?1",
        params![update_id.as_str()],
    )
        .await;
    let eid2 = eid.clone();
    assert!(
        try_primary_rows(
            &store,
            "INSERT INTO guild_scheduled_work(
                    guild_id,enrollment_id,work_id,kind,subject_id,due_at_ms,payload,
                    state,attempt,claim_until_ms,created_at_ms,updated_at_ms
                 ) VALUES(?1,?2,?3,'user_timezone',55,?4,'{}','pending',0,NULL,?4,?4)",
            params![gid, eid2.as_str(), work_id(), now],
            1,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn delete_rules_and_closed_scope_erasure() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    let id = store
        .insert_softban_unban_pending(&scope, 55, "r", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0)
        .await
        .unwrap();
    let delete_id = id.clone();
    assert!(
        try_primary_matching(
            &store,
            "DELETE FROM guild_scheduled_work WHERE work_id=?1",
            params![delete_id.as_str()],
        )
        .await
        .is_err()
    );
    let now = chrono::Utc::now().timestamp_millis();
    let claimed = store
        .claim_scheduled_work_row(
            scope.guild_id(),
            scope.enrollment().id.as_str(),
            &id,
            now,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .finish_scheduled_work(&claimed, "done", None, now)
        .await
        .unwrap();
    primary_reclaim(
        &store,
        "DELETE FROM guild_scheduled_work WHERE work_id=?1",
        params![id.as_str()],
    )
        .await;
    let pending = store
        .insert_softban_unban_pending(&scope, 56, "r", "cccccccccccccccccccccccccccccccc", 0)
        .await
        .unwrap();
    store.close_membership(scope.clone()).await.unwrap();
    while !store.erase_closed_scope(scope.clone()).await.unwrap() {}
    let left: i64 = store
        .db
        .with_conn(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM guild_scheduled_work WHERE work_id=?1",
                params![pending.as_str()],
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn claim_compare_and_swap_and_expired_claim_release() {
    let f = Fixture::new();
    let reg = f
        .registration(
            crate::db::guild_stores::StoreId::new(),
            crate::db::guild_stores::StorePolicy::Shared,
        )
        .await;
    let (store, scope) = bound_scope(&f, &reg, 10).await;
    let _ = store
        .insert_softban_unban_pending(&scope, 55, "r", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", 0)
        .await
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let a = store
        .claim_due_scheduled_work(&["softban_unban"], now, 4)
        .await
        .unwrap();
    assert_eq!(a.len(), 1);
    let b = store
        .claim_due_scheduled_work(&["softban_unban"], now, 4)
        .await
        .unwrap();
    assert!(b.is_empty());
    store
        .release_expired_scheduled_claims(now + crate::db::guild_data::CLAIM_LEASE_MS + 1)
        .await
        .unwrap();
    let read_id = a[0].work_id.clone();
    let state: String = store
        .db
        .with_conn(move |c| {
            c.query_row(
                "SELECT state FROM guild_scheduled_work WHERE work_id=?1",
                params![read_id.as_str()],
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "pending");
}
