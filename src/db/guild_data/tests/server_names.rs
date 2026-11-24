use super::*;
use crate::db::guild_data::server_names::{SERVER_NAME_CHANNEL_LIMIT, SERVER_NAME_ROLE_LIMIT};

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

fn role(id: &str, name: &str, position: i64) -> ServerRoleName {
    ServerRoleName {
        id: id.into(),
        name: name.into(),
        color: 0,
        position,
        managed: false,
    }
}

fn channel(id: &str, name: &str, kind: &str, position: i64) -> ServerChannelName {
    ServerChannelName {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        parent_id: None,
        position,
    }
}

async fn receipts(store: &GuildDataStore) -> i64 {
    store
        .db
        .with_conn(|conn| {
            conn.query_row("SELECT COUNT(*) FROM mutation_receipts", (), |r| {
                Ok(r.get(0)?)
            })
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn server_names_store_a_sorted_bounded_snapshot_and_skip_unchanged_writes() {
    let (_f, store, scope) = ready().await;
    assert_eq!(
        store.server_names(scope.clone()).await.unwrap(),
        ServerNames::default()
    );
    let long = "x".repeat(150);
    let snapshot = ServerNames {
        roles: vec![
            role("9007199254740993", "Members", 1),
            ServerRoleName {
                color: 0xff12_3456,
                ..role("9007199254740995", &long, 5)
            },
            role("9007199254740993", "Members (old)", 1),
        ],
        channels: vec![
            channel("30", "voice-chat", "voice", 2),
            channel("20", "general", "text", 1),
            channel("40", "threads", "public_thread", 3),
        ],
        observed_at_ms: Some(99),
        onboarding_trap: None,
    };
    assert!(
        store
            .replace_server_names(scope.clone(), snapshot.clone(), 1_000)
            .await
            .unwrap()
    );
    let saved = store.server_names(scope.clone()).await.unwrap();
    assert_eq!(saved.observed_at_ms, Some(1_000));
    let roles: Vec<_> = saved.roles.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(roles, ["9007199254740995", "9007199254740993"]);
    assert_eq!(saved.roles[0].name.chars().count(), 100);
    assert_eq!(saved.roles[0].color, 0x12_3456);
    assert_eq!(saved.roles[1].name, "Members");
    let channels: Vec<_> = saved
        .channels
        .iter()
        .map(|c| (c.id.as_str(), c.kind.as_str()))
        .collect();
    assert_eq!(channels, [("20", "text"), ("30", "voice"), ("40", "other")]);

    // The same names (in any order) write nothing, so reconnects stay cheap.
    let before = receipts(&store).await;
    let mut reordered = snapshot.clone();
    reordered.roles.reverse();
    reordered.channels.reverse();
    assert!(
        !store
            .replace_server_names(scope.clone(), reordered, 2_000)
            .await
            .unwrap()
    );
    assert_eq!(receipts(&store).await, before);
    assert_eq!(
        store
            .server_names(scope.clone())
            .await
            .unwrap()
            .observed_at_ms,
        Some(1_000)
    );

    // A deleted role disappears with the next snapshot.
    let mut fewer = snapshot.clone();
    fewer.roles.retain(|r| r.id != "9007199254740993");
    assert!(
        store
            .replace_server_names(scope.clone(), fewer, 3_000)
            .await
            .unwrap()
    );
    let saved = store.server_names(scope.clone()).await.unwrap();
    assert_eq!(saved.roles.len(), 1);
    assert_eq!(saved.roles[0].id, "9007199254740995");

    for invalid in [
        ServerNames {
            roles: vec![role("0", "zero", 0)],
            ..ServerNames::default()
        },
        ServerNames {
            channels: vec![channel("12a", "bad", "text", 0)],
            ..ServerNames::default()
        },
    ] {
        assert!(
            store
                .replace_server_names(scope.clone(), invalid, 4_000)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn onboarding_observations_keep_legacy_names_and_reject_older_updates() {
    let (_f, store, scope) = ready().await;
    let legacy_names = ServerNames {
        roles: vec![role("11", "Trap", 1)],
        ..ServerNames::default()
    };
    assert!(
        store
            .replace_server_names(scope.clone(), legacy_names.clone(), 1_000)
            .await
            .unwrap()
    );
    let before = store.server_names(scope.clone()).await.unwrap();
    assert_eq!(before.roles, legacy_names.roles);
    assert!(before.onboarding_trap.is_none());

    let names = ServerNames {
        onboarding_trap: Some(OnboardingTrapSetup {
            bot_can_kick: Some(true),
            roles_at_or_above_bot: vec![],
            roles_with_grants: vec!["11".into()],
            roles_with_member_grants: vec![],
        }),
        ..legacy_names
    };
    assert!(
        store
            .replace_server_names(scope.clone(), names.clone(), 2_000)
            .await
            .unwrap()
    );
    let mut older = names;
    older.onboarding_trap.as_mut().unwrap().bot_can_kick = Some(false);
    assert!(
        !store
            .replace_server_names(scope.clone(), older, 1_000)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .server_names(scope.clone())
            .await
            .unwrap()
            .onboarding_trap
            .unwrap()
            .bot_can_kick,
        Some(true)
    );

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyNames {
        roles: Vec<ServerRoleName>,
        channels: Vec<ServerChannelName>,
    }
    let encoded: String = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT names FROM guild_server_names WHERE guild_id=10",
                (),
                |row| Ok(row.get(0)?),
            )
        })
        .await
        .unwrap();
    let old: LegacyNames = serde_json::from_str(&encoded).unwrap();
    assert_eq!(old.roles[0].name, "Trap");
    assert!(old.channels.is_empty());

    let current: i64 = store
        .db
        .with_conn(|conn| conn.query_row("PRAGMA user_version", (), |row| Ok(row.get(0)?)))
        .await
        .unwrap();
    assert_eq!(
        current, SCHEMA_VERSION,
        "the refreshed snapshot uses the migrated schema"
    );
    store
        .db
        .with_conn(move |conn| {
            conn.execute_bounded(
                "UPDATE guild_server_names SET names=?1,observed_at_ms=3000 WHERE guild_id=10",
                params![encoded],
                64 * 1024,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let after = store.server_names(scope).await.unwrap();
    assert_eq!(after.roles[0].name, "Trap");
    assert_eq!(after.observed_at_ms, Some(3_000));
    assert!(
        after.onboarding_trap.is_none(),
        "the old writer refreshed names without permission observations"
    );
    drop(
        serde_json::from_value::<ServerNames>(serde_json::json!({
            "roles": [], "channels": [], "future_observation": true
        }))
        .unwrap(),
    );
}

#[tokio::test]
async fn server_names_keep_a_worst_case_snapshot_within_the_row_limit() {
    let (_f, store, scope) = ready().await;
    // Control characters need six-byte JSON escapes, the most a name can grow.
    let name = "\u{1}".repeat(150);
    let names = ServerNames {
        roles: (1..=400)
            .map(|id| role(&id.to_string(), &name, id))
            .collect(),
        channels: (1..=800)
            .map(|id| channel(&(10_000 + id).to_string(), &name, "text", id))
            .collect(),
        observed_at_ms: None,
        onboarding_trap: None,
    };
    assert!(
        store
            .replace_server_names(scope.clone(), names, 1_000)
            .await
            .unwrap()
    );
    let saved = store.server_names(scope).await.unwrap();
    assert_eq!(saved.roles.len(), SERVER_NAME_ROLE_LIMIT);
    assert_eq!(saved.channels.len(), SERVER_NAME_CHANNEL_LIMIT);
    assert_eq!(saved.roles[0].name.chars().count(), 100);
}

#[tokio::test]
async fn server_names_refuse_closed_scopes_and_erasure_removes_them() {
    let (_f, store, scope) = ready().await;
    store
        .replace_server_names(
            scope.clone(),
            ServerNames {
                roles: vec![role("11", "Mods", 1)],
                ..ServerNames::default()
            },
            1_000,
        )
        .await
        .unwrap();
    store.close_membership(scope.clone()).await.unwrap();
    assert!(store.server_names(scope.clone()).await.is_err());
    assert!(
        store
            .replace_server_names(scope.clone(), ServerNames::default(), 2_000)
            .await
            .is_err()
    );
    assert!(store.erase_closed_scope(scope.clone()).await.unwrap());
    let left: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM guild_server_names WHERE guild_id=10",
                (),
                |r| Ok(r.get(0)?),
            )
        })
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn version_nineteen_store_upgrades_to_server_names() {
    let f = Fixture::new();
    let reg = f.registration(StoreId::new(), StorePolicy::Shared).await;
    GuildDataStore::create_historical_store(f.options(), reg.clone(), 19).unwrap();
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
    assert!(
        store
            .replace_server_names(
                scope.clone(),
                ServerNames {
                    channels: vec![channel("20", "general", "text", 0)],
                    ..ServerNames::default()
                },
                1_000,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.server_names(scope).await.unwrap().channels[0].name,
        "general"
    );
}
