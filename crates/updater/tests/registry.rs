use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, Uri},
    response::Response,
    routing::get,
};
use jmodguard_updater::oci::Registry;
use jmodguard_updater::signature::{
    BAD_IMAGE_SIGNATURE, EXPIRED_IMAGE_SIGNATURE, UNSIGNED_IMAGE, UNTRUSTED_IMAGE_SIGNATURE_KEY,
    signature_rejection_phrase, signature_tag_for_digest,
};
use jmodguard_updater::signing_fixture::{
    SigningFixture, SigningKeyChoice, image as signed_image, signature_not_after_far_future,
};
use std::{collections::HashMap, sync::Arc, sync::OnceLock};
use tokio::sync::RwLock;

#[derive(Default)]
struct RegistryFixture {
    tag: String,
    objects: HashMap<String, Vec<u8>>,
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

fn image(fixture: &mut RegistryFixture, binary: &[u8], channel: &str, source: char) -> String {
    let mut objects = fixture.objects.clone();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        binary,
        channel,
        &signature_not_after_far_future(),
        source,
    );
    fixture.objects = objects;
    release.digest
}

async fn start_registry(
    fixture: RegistryFixture,
) -> (
    Registry,
    Arc<RwLock<RegistryFixture>>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(RwLock::new(fixture));
    let router = Router::new()
        .fallback(get(serve_object))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let registry = Registry::new(&format!("http://{addr}")).unwrap();
    (registry, state, server)
}

#[tokio::test]
async fn channel_move_keeps_approved_digest_and_corrupt_blobs_fail() {
    let mut fixture = RegistryFixture::default();
    let old = image(&mut fixture, b"\x7fELFfirst", "development", 'a');
    let new = image(&mut fixture, b"\x7fELFsecond", "development", 'b');
    let stable = image(&mut fixture, b"\x7fELFstable", "stable", 'c');
    let nightly = image(&mut fixture, b"\x7fELFnightly", "nightly", 'd');
    fixture.tag = old.clone();
    let (registry, state, server) = start_registry(fixture).await;
    let approved = registry.discover("api", "latest").await.unwrap();
    assert_eq!(approved.channel, "latest");
    assert_eq!(approved.launcher.as_ref().unwrap().revision, 19);
    assert_eq!(approved.required_launcher(), 19);
    assert!(approved.supports_launcher(18).is_err());
    approved.supports_launcher(19).unwrap();
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
    assert_eq!(
        registry.discover("api", "latest").await.unwrap().digest,
        new
    );
    state.write().await.tag = stable.clone();
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
    server.abort();
}

#[tokio::test]
async fn signature_policy_rejects_bad_missing_expired_and_unknown_keys() {
    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        b"\x7fELFsigned",
        "development",
        &signature_not_after_far_future(),
        'a',
    );
    let tag = signature_tag_for_digest(&release.digest).unwrap();
    let (registry, state, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    registry.discover("api", "latest").await.unwrap();
    state.write().await.objects.remove(&tag);
    let error = registry.discover("api", "latest").await.unwrap_err();
    assert!(error.to_string().contains(UNSIGNED_IMAGE));
    server.abort();

    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        b"\x7fELFsigned",
        "development",
        "2020-01-01T00:00:00Z",
        'e',
    );
    let (registry, _, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    let error = registry.discover("api", "latest").await.unwrap_err();
    assert!(error.to_string().contains(EXPIRED_IMAGE_SIGNATURE));
    server.abort();

    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        b"\x7fELFsigned",
        "development",
        "2020-01-01T00:00:00Z",
        'b',
    );
    signing().sign(
        &mut objects,
        "api",
        &release.digest,
        &signature_not_after_far_future(),
        SigningKeyChoice::Current,
    );
    let (registry, _, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    registry.discover("api", "latest").await.unwrap();
    server.abort();

    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        b"\x7fELFsigned",
        "development",
        &signature_not_after_far_future(),
        'a',
    );
    let tag = signature_tag_for_digest(&release.digest).unwrap();
    objects.insert(tag, b"not-json".to_vec());
    let (registry, _, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    let error = registry.discover("api", "latest").await.unwrap_err();
    assert!(error.to_string().contains(BAD_IMAGE_SIGNATURE));
    server.abort();

    let rotation = signing().with_previous_key();
    rotation.apply_env();
    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        &rotation,
        "api",
        b"\x7fELFsigned",
        "development",
        &signature_not_after_far_future(),
        'f',
    );
    rotation.sign(
        &mut objects,
        "api",
        &release.digest,
        &signature_not_after_far_future(),
        SigningKeyChoice::Previous,
    );
    let (registry, _, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    registry.discover("api", "latest").await.unwrap();
    server.abort();

    let mut objects = HashMap::new();
    let release = signed_image(
        &mut objects,
        signing(),
        "api",
        b"\x7fELFsigned",
        "development",
        &signature_not_after_far_future(),
        'g',
    );
    let tag = signature_tag_for_digest(&release.digest).unwrap();
    objects.remove(&tag);
    signing().sign(
        &mut objects,
        "api",
        &release.digest,
        &signature_not_after_far_future(),
        SigningKeyChoice::Unknown,
    );
    let (registry, _, server) = start_registry(RegistryFixture {
        tag: release.digest.clone(),
        objects,
    })
    .await;
    let error = registry.discover("api", "latest").await.unwrap_err();
    assert_eq!(
        signature_rejection_phrase(&error),
        Some(UNTRUSTED_IMAGE_SIGNATURE_KEY)
    );
    server.abort();
}
