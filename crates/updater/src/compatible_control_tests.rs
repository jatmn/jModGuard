//! Admission tests for [`compatible_control`].
use super::compatible_control;
use jmodguard::db::{
    ComponentRelease, LauncherCompatibility, UPDATE_CONTROL_PROTOCOL, UpdateControl,
    UpgradeCompatibility,
};

async fn observed_fixture() -> (UpdateControl, ComponentRelease) {
    let path = std::env::temp_dir().join(format!(
        "compatible-control-{:032x}.db",
        rand::random::<u128>()
    ));
    let db = jmodguard::db::Db::open(&path, 2).unwrap();
    let opts = jmodguard::db::DbOpenOptions {
        read_only: false,
        path,
        url: String::new(),
        token: String::new(),
        pool_size: 2,
        migrate: false,
        default_prefix: "!".into(),
        retention_days: 1,
    };
    let control = UpdateControl::open(&opts).unwrap().unwrap();
    let core = control.schema_version().await.unwrap();
    let guild = jmodguard::db::guild_data::SCHEMA_VERSION;
    let release = ComponentRelease {
        component: "bot".into(),
        channel: "latest".into(),
        release_id: "fixture".into(),
        source_sha: "c".repeat(40),
        published_at: "2026-10-01T00:00:00Z".into(),
        image: "ghcr.io/jatmn/jmodguard-bot".into(),
        digest: format!("sha256:{}", "a".repeat(64)),
        min_schema: core,
        max_schema: core,
        result_schema: core,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(LauncherCompatibility {
            revision: jmodguard::db::LAUNCHER_REVISION,
            min_revision: jmodguard::db::REQUIRED_LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(UpgradeCompatibility {
            control_protocol: UPDATE_CONTROL_PROTOCOL,
            min_core: 15,
            min_guild: 1,
            guild_schema: guild,
        }),
    };
    for (role, digest) in [("bot", 'a'), ("api", 'b')] {
        let mut row = release.clone();
        row.component = role.into();
        row.digest = format!("sha256:{}", digest.to_string().repeat(64));
        row.image = format!("ghcr.io/jatmn/jmodguard-{role}");
        control
            .checked(role, "latest", Some(row), None, 100)
            .await
            .unwrap();
        control
            .running(
                role,
                None,
                jmodguard::db::ComponentObservation {
                    source_sha: "d".repeat(40),
                    executable_sha256: "e".repeat(64),
                    admin_sha256: None,
                    base_image_sha: "d".repeat(40),
                    started_at: 90,
                    min_schema: core,
                    max_schema: core,
                    api_capability: 1,
                    self_update_enabled: true,
                    base_abi: "debian-bookworm-amd64".into(),
                    guild_schema: Some(guild),
                    update_protocol: UPDATE_CONTROL_PROTOCOL,
                    launcher_revision: jmodguard::db::LAUNCHER_REVISION,
                },
                100,
            )
            .await
            .unwrap();
    }
    drop(db);
    (control, release)
}

#[tokio::test(flavor = "multi_thread")]
async fn update_control_refuses_higher_result_schema() {
    let (control, release) = observed_fixture().await;
    let schema = control.schema_version().await.unwrap();
    let mut candidate = release;
    candidate.max_schema = schema + 1;
    candidate.result_schema = schema + 1;
    let error = compatible_control(&control, &candidate, 200)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        jmodguard::db::APP_ONLY_SCHEMA_CHANGE_REFUSED
    );
    assert_eq!(control.schema_version().await.unwrap(), schema);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_control_refuses_guild_schema_change() {
    let (control, release) = observed_fixture().await;
    let mut candidate = release;
    candidate.upgrade.as_mut().unwrap().guild_schema =
        jmodguard::db::guild_data::SCHEMA_VERSION + 1;
    let error = compatible_control(&control, &candidate, 200)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        jmodguard::db::APP_ONLY_SCHEMA_CHANGE_REFUSED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn update_control_accepts_equal_schema() {
    let (control, release) = observed_fixture().await;
    compatible_control(&control, &release, 200)
        .await
        .expect("equal schema should be admitted");
}

#[test]
fn app_only_schema_refuse_requires_launcher_revision_eighteen() {
    assert_eq!(jmodguard::db::LAUNCHER_REVISION, 19);
}
