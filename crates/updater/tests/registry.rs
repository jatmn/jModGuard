use axum::{
    Router,
    body::Body,
    extract::State,
    http::{StatusCode, Uri},
    response::Response,
    routing::get,
};
use jmodguard_updater::oci::{Registry, sha256_hex};
use serde_json::json;
use std::{collections::HashMap, io::Write, sync::Arc};
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

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
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
