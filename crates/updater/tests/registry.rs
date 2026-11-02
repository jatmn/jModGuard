use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, Uri},
    response::Response,
    routing::get,
};
use jmodguard::db::ComponentRelease;
// The source modules compile with cfg(test), keeping fixture trust out of the
// normal library that the production-trust regression also exercises below.
#[path = "../src/oci.rs"]
pub mod oci;
#[path = "../src/signature.rs"]
pub mod signature;
#[path = "../src/signing_fixture.rs"]
pub mod signing_fixture;
use oci::Registry;
use serde_json::json;
use signature::signature_rejection_phrase;
use signing_fixture::{
    BadSignature, SigningFixture, SigningKeyChoice, signature_not_after_far_future,
};
use std::sync::{Arc, OnceLock};
use std::{collections::HashMap, io::Write};
use tokio::sync::RwLock;

#[derive(Default)]
struct RegistryFixture {
    tag: String,
    objects: HashMap<String, Vec<u8>>,
    delayed_payload: Option<(String, chrono::DateTime<chrono::Utc>)>,
}

async fn serve_object(State(state): State<Arc<RwLock<RegistryFixture>>>, uri: Uri) -> Response {
    let path = uri.path();
    let fixture = state.read().await;
    let reference = path.rsplit('/').next().unwrap_or("");
    let key = if matches!(reference, "latest" | "stable") {
        &fixture.tag
    } else {
        reference
    };
    if let Some((delayed, expires)) = &fixture.delayed_payload
        && key == delayed
    {
        let now = chrono::Utc::now();
        assert!(now < *expires, "payload request must start before expiry");
        tokio::time::sleep((*expires - now).to_std().unwrap()).await;
    }
    match fixture.objects.get(key) {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(bytes.clone()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap(),
    }
}

fn signing() -> &'static SigningFixture {
    static FIXTURE: OnceLock<SigningFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let fixture = SigningFixture::new();
        fixture.apply_env();
        fixture
    })
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", jmodguard_updater::oci::sha256_hex(bytes))
}

fn image(fixture: &mut RegistryFixture, source: char, binary: &[u8], channel: &str) -> String {
    let sha = source.to_string().repeat(40);
    let config = serde_json::to_vec(&json!({
        "architecture":"amd64","os":"linux","config":{"Labels":{
            "io.jmodguard.component":"api","io.jmodguard.channel":channel,
            "io.jmodguard.release-id":format!("v1.0.{source}"),
            "io.jmodguard.published-at":"2026-09-23T00:00:00Z",
            "io.jmodguard.min-schema":"15","io.jmodguard.max-schema":"15",
            "io.jmodguard.result-schema":"15","io.jmodguard.min-api":"1",
            "io.jmodguard.max-api":"1","io.jmodguard.launcher-protocol":"1",
            "io.jmodguard.base-abi":"debian-bookworm-amd64",
            "io.jmodguard.update-control":"3", "io.jmodguard.upgrade-min-core":"15",
            "io.jmodguard.upgrade-min-guild":"1", "io.jmodguard.guild-schema":"14",
            "io.jmodguard.launcher-revision":"7", "io.jmodguard.min-launcher-revision":"4",
            "org.opencontainers.image.revision":sha
        }}
    }))
    .unwrap();
    let mut tar_bytes = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "usr/local/bin/jmodguard-api", binary)
            .unwrap();
        tar.finish().unwrap();
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
    let layer = encoder.finish().unwrap();
    let config_digest = digest(&config);
    let layer_digest = digest(&layer);
    let manifest=serde_json::to_vec(&json!({
        "schemaVersion":2,
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_digest,"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":layer_digest,"size":layer.len()}]
    })).unwrap();
    fixture.objects.insert(config_digest, config);
    fixture.objects.insert(layer_digest, layer);
    let manifest_digest = digest(&manifest);
    fixture.objects.insert(sha, manifest.clone());
    fixture.objects.insert(manifest_digest.clone(), manifest);
    signing().sign(
        &mut fixture.objects,
        "api",
        &manifest_digest,
        &signature_not_after_far_future(),
        SigningKeyChoice::Current,
    );
    manifest_digest
}

#[tokio::test]
async fn channel_move_keeps_approved_digest_and_corrupt_blobs_fail() {
    let mut fixture = RegistryFixture::default();
    let old = image(&mut fixture, 'a', b"\x7fELFfirst", "development");
    let new = image(&mut fixture, 'b', b"\x7fELFsecond", "development");
    let stable = image(&mut fixture, 'c', b"\x7fELFstable", "stable");
    let nightly = image(&mut fixture, 'd', b"\x7fELFnightly", "nightly");
    fixture.tag = old.clone();
    let state = Arc::new(RwLock::new(fixture));
    let router = Router::new()
        .fallback(get(serve_object))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let registry = Registry::new(&format!("http://{addr}")).unwrap();
    let approved = registry.discover("api", "latest").await.unwrap();
    assert_eq!(approved.channel, "latest");
    assert_eq!(approved.launcher.as_ref().unwrap().revision, 7);
    assert_eq!(approved.required_launcher(), 4);
    assert!(approved.supports_launcher(3).is_err());
    approved.supports_launcher(4).unwrap();
    approved.supports_launcher(9).unwrap();
    assert_eq!(approved.digest, old);
    state.write().await.tag = new.clone();
    assert_eq!(
        registry
            .discover_source("api", "latest", &"a".repeat(40))
            .await
            .unwrap()
            .digest,
        old
    );
    assert!(
        registry
            .discover_source("api", "latest", &"f".repeat(40))
            .await
            .is_err()
    );
    assert!(
        registry
            .discover_source("api", "latest", "../../other-image")
            .await
            .is_err()
    );

    assert_eq!(
        registry.discover("api", "latest").await.unwrap().digest,
        new
    );
    state.write().await.tag = stable.clone();
    assert_eq!(
        registry.discover("api", "latest").await.unwrap().digest,
        stable
    );
    assert_eq!(
        registry.discover("api", "stable").await.unwrap().digest,
        stable
    );
    state.write().await.tag = nightly;
    assert!(registry.discover("api", "latest").await.is_err());
    assert_eq!(
        registry
            .files(&approved, &["usr/local/bin/jmodguard-api"])
            .await
            .unwrap()["usr/local/bin/jmodguard-api"],
        b"\x7fELFfirst"
    );
    let wrong_revision_manifest = state.read().await.objects[&new].clone();
    state
        .write()
        .await
        .objects
        .insert("a".repeat(40), wrong_revision_manifest);
    assert!(
        registry
            .discover_source("api", "latest", &"a".repeat(40))
            .await
            .is_err()
    );
    let config_digest = {
        let guard = state.read().await;
        let manifest: serde_json::Value = serde_json::from_slice(&guard.objects[&old]).unwrap();
        manifest["config"]["digest"].as_str().unwrap().to_owned()
    };
    state
        .write()
        .await
        .objects
        .insert(config_digest, b"altered config".to_vec());
    assert!(
        registry
            .files(&approved, &["usr/local/bin/jmodguard-api"])
            .await
            .is_err()
    );
    server.abort();
}

fn api_release(digest: &str, source: char) -> ComponentRelease {
    ComponentRelease {
        component: "api".into(),
        channel: "latest".into(),
        release_id: format!("v1.0.{source}"),
        source_sha: source.to_string().repeat(40),
        published_at: "2026-09-23T00:00:00Z".into(),
        image: "ghcr.io/jatmn/jmodguard-api".into(),
        digest: digest.into(),
        min_schema: 15,
        max_schema: 15,
        result_schema: 15,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(jmodguard::db::LauncherCompatibility {
            revision: 7,
            min_revision: 4,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(jmodguard::db::UpgradeCompatibility {
            control_protocol: 3,
            min_core: 15,
            min_guild: 1,
            guild_schema: 14,
        }),
    }
}

async fn serve(fixture: RegistryFixture) -> (Registry, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .fallback(get(serve_object))
        .with_state(Arc::new(RwLock::new(fixture)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (Registry::new(&format!("http://{addr}")).unwrap(), server)
}

#[tokio::test]
async fn signature_policy_refuses_before_staging_download() {
    for (index, bad) in BadSignature::ALL.into_iter().enumerate() {
        let mut fixture = RegistryFixture::default();
        let source = char::from(b'a' + index as u8);
        let digest = image(&mut fixture, source, b"\x7fELFsigned", "development");
        signing().damage(&mut fixture.objects, "api", &digest, bad);
        if matches!(bad, BadSignature::Wrong) {
            let tag = jmodguard_updater::signature::signature_tag_for_digest(&digest).unwrap();
            let body: serde_json::Value = serde_json::from_slice(&fixture.objects[&tag]).unwrap();
            assert!(body.get("payloadType").is_none());
            assert!(
                body["layers"][0]["annotations"]["dev.cosignproject.cosign/signature"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
        }
        fixture.tag = digest.clone();
        let release = api_release(&digest, source);
        let (registry, server) = serve(fixture).await;
        for error in [
            registry.discover("api", "latest").await.unwrap_err(),
            registry
                .files(&release, &["usr/local/bin/jmodguard-api"])
                .await
                .unwrap_err(),
        ] {
            assert_eq!(error.to_string(), bad.phrase(), "{error:?}");
            assert_eq!(signature_rejection_phrase(&error), Some(bad.phrase()));
        }
        server.abort();
    }
}

#[tokio::test]
async fn expired_cosign_layer_does_not_hide_an_older_valid_signature() {
    let mut fixture = RegistryFixture::default();
    let digest = image(&mut fixture, 'e', b"\x7fELFsigned", "development");
    for _ in 0..9 {
        signing().sign(
            &mut fixture.objects,
            "api",
            &digest,
            "2020-01-01T00:00:00Z",
            SigningKeyChoice::Current,
        );
    }
    let release = api_release(&digest, 'e');
    fixture.tag = digest;
    let (registry, server) = serve(fixture).await;
    assert_eq!(
        registry
            .files(&release, &["usr/local/bin/jmodguard-api"])
            .await
            .unwrap()["usr/local/bin/jmodguard-api"],
        b"\x7fELFsigned"
    );
    server.abort();
}

#[tokio::test]
async fn damaged_cosign_payloads_are_skipped_and_report_signature_rejection() {
    for missing in [true, false] {
        for valid_older in [true, false] {
            let mut fixture = RegistryFixture::default();
            let digest = image(&mut fixture, 'e', b"\x7fELFsigned", "development");
            let tag = jmodguard_updater::signature::signature_tag_for_digest(&digest).unwrap();
            if !valid_older {
                fixture.objects.remove(&tag);
            }
            signing().sign(
                &mut fixture.objects,
                "api",
                &digest,
                "2020-01-01T00:00:00Z",
                SigningKeyChoice::Current,
            );
            let manifest: serde_json::Value =
                serde_json::from_slice(&fixture.objects[&tag]).unwrap();
            let payload = manifest["layers"].as_array().unwrap().last().unwrap()["digest"]
                .as_str()
                .unwrap();
            if missing {
                fixture.objects.remove(payload);
            } else {
                fixture.objects.insert(payload.into(), b"damaged".to_vec());
            }
            let release = api_release(&digest, 'e');
            fixture.tag = digest;
            let (registry, server) = serve(fixture).await;
            let discovered = registry.discover("api", "latest").await;
            let files = registry
                .files(&release, &["usr/local/bin/jmodguard-api"])
                .await;
            if valid_older {
                assert_eq!(discovered.unwrap().digest, release.digest);
                assert_eq!(
                    files.unwrap()["usr/local/bin/jmodguard-api"],
                    b"\x7fELFsigned"
                );
            } else {
                for error in [discovered.unwrap_err(), files.unwrap_err()] {
                    assert_eq!(
                        signature_rejection_phrase(&error),
                        Some("bad image signature")
                    );
                }
            }
            server.abort();
        }
    }
}

#[tokio::test]
async fn cosign_payload_fetch_crossing_expiry_is_refused() {
    let mut fixture = RegistryFixture::default();
    let digest = image(&mut fixture, 'e', b"\x7fELFsigned", "development");
    let tag = jmodguard_updater::signature::signature_tag_for_digest(&digest).unwrap();
    fixture.objects.remove(&tag);
    let expires = chrono::Utc::now() + chrono::Duration::seconds(3);
    signing().sign(
        &mut fixture.objects,
        "api",
        &digest,
        &expires.to_rfc3339(),
        SigningKeyChoice::Current,
    );
    let manifest: serde_json::Value = serde_json::from_slice(&fixture.objects[&tag]).unwrap();
    fixture.delayed_payload = Some((
        manifest["layers"][0]["digest"].as_str().unwrap().into(),
        expires,
    ));
    fixture.tag = digest;
    let (registry, server) = serve(fixture).await;
    let result = registry.discover("api", "latest").await;
    server.abort();
    assert_eq!(
        signature_rejection_phrase(&result.unwrap_err()),
        Some("expired image signature")
    );
}

#[test]
fn normal_library_ignores_test_signing_pem() {
    let fixture = signing();
    let manifest = b"manifest-bytes";
    let digest = digest(manifest);
    let envelope = signature::test_support::sign_manifest(
        &fixture.private_pem,
        &fixture.public_pem,
        "api",
        &digest,
        &signature_not_after_far_future(),
    );
    let result = jmodguard_updater::signature::verify_manifest_signature(
        "api",
        &digest,
        manifest,
        &envelope,
        chrono::Utc::now(),
        jmodguard_updater::signature::SignatureExpiry::Enforce,
    );
    assert_eq!(
        jmodguard_updater::signature::signature_rejection_phrase(&result.unwrap_err()),
        Some("image signature used an untrusted key")
    );
}
