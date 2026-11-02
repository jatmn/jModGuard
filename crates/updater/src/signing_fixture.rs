//! Small registry fixture for signature verification tests.
use crate::signature::signature_tag_for_digest;
use crate::signature::test_support::{
    generate_key_pair, sign_manifest, sign_manifest_cosign, tamper_cosign_annotation,
};
use chrono::{Duration, Utc};
use std::collections::HashMap;

pub fn signature_not_after_far_future() -> String {
    (Utc::now() + Duration::days(3650))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

pub struct SigningFixture {
    pub public_pem: String,
    pub private_pem: String,
}

impl Default for SigningFixture {
    fn default() -> Self {
        let (public_pem, private_pem) = generate_key_pair();
        Self {
            public_pem,
            private_pem,
        }
    }
}

impl SigningFixture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_env(&self) {
        unsafe {
            std::env::set_var("JMODGUARD_TEST_UPDATE_SIGNING_PUBLIC_PEM", &self.public_pem);
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
        let (public_pem, private_pem) = match key {
            SigningKeyChoice::Current => (self.public_pem.as_str(), self.private_pem.as_str()),
            SigningKeyChoice::Unknown => {
                let (public_pem, private_pem) = generate_key_pair();
                self.store(
                    objects,
                    &sign_manifest_cosign(
                        &private_pem,
                        &public_pem,
                        component,
                        manifest_digest,
                        not_after,
                    ),
                    manifest_digest,
                    false,
                );
                return;
            }
        };
        self.store(
            objects,
            &sign_manifest_cosign(
                private_pem,
                public_pem,
                component,
                manifest_digest,
                not_after,
            ),
            manifest_digest,
            true,
        );
    }

    fn store(
        &self,
        objects: &mut HashMap<String, Vec<u8>>,
        signed: &(Vec<u8>, Vec<(String, Vec<u8>)>),
        manifest_digest: &str,
        append: bool,
    ) {
        let (manifest, blobs) = signed;
        let tag = signature_tag_for_digest(manifest_digest).unwrap();
        for (digest, bytes) in blobs {
            objects.insert(digest.clone(), bytes.clone());
        }
        if append
            && let Some(existing) = objects.get(&tag)
            && let (Ok(mut existing_manifest), Ok(new_manifest)) = (
                serde_json::from_slice::<serde_json::Value>(existing),
                serde_json::from_slice::<serde_json::Value>(manifest),
            )
            && let (Some(old_layers), Some(new_layers)) = (
                existing_manifest
                    .get_mut("layers")
                    .and_then(|value| value.as_array_mut()),
                new_manifest
                    .get("layers")
                    .and_then(|value| value.as_array()),
            )
        {
            old_layers.extend(new_layers.iter().cloned());
            objects.insert(tag, serde_json::to_vec(&existing_manifest).unwrap());
            return;
        }
        objects.insert(tag, manifest.clone());
    }
}

pub enum SigningKeyChoice {
    Current,
    Unknown,
}

#[derive(Clone, Copy)]
pub enum BadSignature {
    Unsigned,
    Wrong,
    Expired,
    Untrusted,
}

impl BadSignature {
    pub const ALL: [Self; 4] = [Self::Unsigned, Self::Wrong, Self::Expired, Self::Untrusted];

    pub const fn phrase(self) -> &'static str {
        match self {
            Self::Unsigned => crate::signature::UNSIGNED_IMAGE,
            Self::Wrong => crate::signature::BAD_IMAGE_SIGNATURE,
            Self::Expired => crate::signature::EXPIRED_IMAGE_SIGNATURE,
            Self::Untrusted => crate::signature::UNTRUSTED_IMAGE_SIGNATURE_KEY,
        }
    }
}

impl SigningFixture {
    /// Replace a good signature. Image config and layer blobs are removed so a
    /// refusal that still names the signature did not download them.
    pub fn damage(
        &self,
        objects: &mut HashMap<String, Vec<u8>>,
        component: &str,
        manifest_digest: &str,
        bad: BadSignature,
    ) {
        let manifest: serde_json::Value =
            serde_json::from_slice(&objects[manifest_digest]).unwrap();
        for digest in [
            manifest["config"]["digest"].as_str().unwrap(),
            manifest["layers"][0]["digest"].as_str().unwrap(),
        ] {
            objects.remove(digest);
        }
        let tag = signature_tag_for_digest(manifest_digest).unwrap();
        let future = signature_not_after_far_future();
        match bad {
            BadSignature::Unsigned => {
                objects.remove(&tag);
            }
            BadSignature::Wrong => {
                let manifest = objects[&tag].clone();
                objects.insert(tag, tamper_cosign_annotation(&manifest));
            }
            BadSignature::Expired => {
                objects.remove(&tag);
                self.sign(
                    objects,
                    component,
                    manifest_digest,
                    "2020-01-01T00:00:00Z",
                    SigningKeyChoice::Current,
                );
            }
            BadSignature::Untrusted => {
                // The annotation path cannot name a signer. Untrusted means the
                // envelope carries a key id that is not the pinned key.
                objects.remove(&tag);
                let (public_pem, private_pem) = generate_key_pair();
                objects.insert(
                    tag,
                    sign_manifest(
                        &private_pem,
                        &public_pem,
                        component,
                        manifest_digest,
                        &future,
                    ),
                );
            }
        }
    }
}
