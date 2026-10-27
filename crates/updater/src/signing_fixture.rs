//! Registry fixture helpers for signature verification tests.
use crate::oci::sha256_hex;
use crate::signature::signature_tag_for_digest;
use crate::signature::test_support::{generate_key_pair, sign_manifest};
use jmodguard::db::ComponentRelease;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;

pub struct SigningFixture {
    pub public_pem: String,
    pub private_pem: String,
    pub previous_public_pem: Option<String>,
    pub previous_private_pem: Option<String>,
}

impl Default for SigningFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl SigningFixture {
    pub fn new() -> Self {
        let (public_pem, private_pem) = generate_key_pair();
        Self {
            public_pem,
            private_pem,
            previous_public_pem: None,
            previous_private_pem: None,
        }
    }

    pub fn with_previous_key(&self) -> Self {
        let (public_pem, private_pem) = generate_key_pair();
        Self {
            public_pem: self.public_pem.clone(),
            private_pem: self.private_pem.clone(),
            previous_public_pem: Some(public_pem),
            previous_private_pem: Some(private_pem),
        }
    }

    pub fn apply_env(&self) {
        unsafe {
            std::env::set_var("JMODGUARD_TEST_UPDATE_SIGNING_PUBLIC_PEM", &self.public_pem);
        }
        if let Some(public) = self.previous_public_pem.as_ref() {
            let root = std::env::var("JMODGUARD_REPO_ROOT").unwrap_or_else(|_| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .to_string_lossy()
                    .into_owned()
            });
            let deploy = std::path::Path::new(&root).join("deploy");
            std::fs::create_dir_all(&deploy).unwrap();
            std::fs::write(deploy.join("update-signing-previous.pub"), public).unwrap();
        }
    }

    pub fn sign(
        &self,
        objects: &mut HashMap<String, Vec<u8>>,
        component: &str,
        manifest_digest: &str,
        not_after: &str,
        key: SigningKeyChoice,
    ) {
        let (public, private) = match key {
            SigningKeyChoice::Current => (&self.public_pem, &self.private_pem),
            SigningKeyChoice::Previous => (
                self.previous_public_pem.as_ref().expect("previous key"),
                self.previous_private_pem.as_ref().expect("previous key"),
            ),
            SigningKeyChoice::Unknown => {
                let (public, private) = generate_key_pair();
                let body = sign_manifest(&private, &public, component, manifest_digest, not_after);
                let tag = signature_tag_for_digest(manifest_digest).unwrap();
                objects.insert(tag, body);
                return;
            }
        };
        let body = sign_manifest(private, public, component, manifest_digest, not_after);
        let tag = signature_tag_for_digest(manifest_digest).unwrap();
        objects.insert(tag, body);
    }
}

pub enum SigningKeyChoice {
    Current,
    Previous,
    Unknown,
}

pub fn image(
    objects: &mut HashMap<String, Vec<u8>>,
    fixture: &SigningFixture,
    role: &str,
    binary: &[u8],
    channel: &str,
    not_after: &str,
    source_seed: char,
) -> ComponentRelease {
    let sha = source_seed.to_string().repeat(40);
    let config = serde_json::to_vec(&json!({
        "architecture":"amd64","os":"linux","config":{"Labels":{
            "io.jmodguard.component":role,"io.jmodguard.channel":channel,
            "io.jmodguard.release-id":"sig-fixture",
            "io.jmodguard.published-at":"2026-09-23T00:00:00Z",
            "io.jmodguard.min-schema":"15","io.jmodguard.max-schema":"15",
            "io.jmodguard.result-schema":"15","io.jmodguard.min-api":"1",
            "io.jmodguard.max-api":"1","io.jmodguard.launcher-protocol":"1",
            "io.jmodguard.base-abi":"debian-bookworm-amd64",
            "io.jmodguard.update-control":"3", "io.jmodguard.upgrade-min-core":"15",
            "io.jmodguard.upgrade-min-guild":"1", "io.jmodguard.guild-schema":"14",
            "io.jmodguard.launcher-revision":"18", "io.jmodguard.min-launcher-revision":"18",
            "org.opencontainers.image.revision":sha
        }}
    }))
    .unwrap();
    let mut tar_bytes = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_bytes);
        let name = format!("usr/local/bin/jmodguard-{role}");
        let inventory = format!("{hash}  bin/jmodguard-{role}\n", hash = sha256_hex(binary));
        for (path, bytes) in [
            (name.as_str(), binary),
            (
                "usr/local/share/jmodguard/files.sha256",
                inventory.as_bytes(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        tar.finish().unwrap();
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).unwrap();
    let layer = encoder.finish().unwrap();
    let digest = |bytes: &[u8]| format!("sha256:{}", sha256_hex(bytes));
    let config_digest = digest(&config);
    let layer_digest = digest(&layer);
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion":2,
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_digest,"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":layer_digest,"size":layer.len()}]
    }))
    .unwrap();
    objects.insert(config_digest, config);
    objects.insert(layer_digest, layer);
    let manifest_digest = digest(&manifest);
    objects.insert(sha.clone(), manifest.clone());
    objects.insert(manifest_digest.clone(), manifest);
    fixture.sign(
        objects,
        role,
        &manifest_digest,
        not_after,
        SigningKeyChoice::Current,
    );
    ComponentRelease {
        component: role.into(),
        channel: "latest".into(),
        release_id: "sig-fixture".into(),
        source_sha: sha,
        published_at: "2026-09-23T00:00:00Z".into(),
        image: format!("ghcr.io/jatmn/jmodguard-{role}"),
        digest: manifest_digest,
        min_schema: 15,
        max_schema: 15,
        result_schema: 15,
        min_api: 1,
        max_api: 1,
        launcher_protocol: 1,
        launcher: Some(jmodguard::db::LauncherCompatibility {
            revision: jmodguard::db::LAUNCHER_REVISION,
            min_revision: jmodguard::db::REQUIRED_LAUNCHER_REVISION,
        }),
        base_abi: "debian-bookworm-amd64".into(),
        upgrade: Some(jmodguard::db::UpgradeCompatibility {
            control_protocol: jmodguard::db::UPDATE_CONTROL_PROTOCOL,
            min_core: 15,
            min_guild: 1,
            guild_schema: 14,
        }),
    }
}
