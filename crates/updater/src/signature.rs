//! Cosign simple-signing verification for OCI release manifests.
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use ring::digest::{SHA256, digest};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;

pub const UNSIGNED_IMAGE: &str = "unsigned image";
pub const BAD_IMAGE_SIGNATURE: &str = "bad image signature";
pub const EXPIRED_IMAGE_SIGNATURE: &str = "expired image signature";
pub const UNTRUSTED_IMAGE_SIGNATURE_KEY: &str = "image signature used an untrusted key";

/// New installs refuse a signature at or after `io.jmodguard.not-after`.
/// Boot repair of an already-installed release may accept that same signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureExpiry {
    Enforce,
    AllowExpired,
}

const CURRENT_PUBLIC_PEM: &str = include_str!("../../../deploy/update-signing.pub");
const PAYLOAD_TYPE: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
const SIGNATURE_TYPE: &str = "cosign container image signature";

#[derive(Debug)]
pub struct ImageSignatureRejection {
    pub phrase: &'static str,
}

impl std::fmt::Display for ImageSignatureRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.phrase)
    }
}

impl std::error::Error for ImageSignatureRejection {}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "payloadType")]
    payload_type: String,
    payload: String,
    signatures: Vec<SignatureEntry>,
}

#[derive(Debug, Deserialize)]
struct SignatureEntry {
    keyid: String,
    sig: String,
}

#[derive(Debug, Deserialize)]
struct SimpleSigningPayload {
    critical: Critical,
    optional: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct Critical {
    #[serde(rename = "type")]
    kind: String,
    identity: Identity,
    image: ImageRef,
}

#[derive(Debug, Deserialize)]
struct Identity {
    #[serde(rename = "docker-reference")]
    docker_reference: String,
}

#[derive(Debug, Deserialize)]
struct ImageRef {
    #[serde(rename = "docker-manifest-digest")]
    docker_manifest_digest: String,
}

struct TrustedKey {
    keyid: String,
    key: UnparsedPublicKey<Vec<u8>>,
}

fn decode_pem_spki(pem: &str) -> Result<Vec<u8>> {
    let body = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<String>();
    STANDARD
        .decode(body)
        .context("invalid signing public key PEM")
}

fn sec1_p256_public_key(der: &[u8]) -> Result<Vec<u8>> {
    if der.len() == 65 && der.first() == Some(&0x04) {
        return Ok(der.to_vec());
    }
    ensure!(
        der.len() >= 65 && der[der.len() - 65] == 0x04,
        "invalid signing public key"
    );
    Ok(der[der.len() - 65..].to_vec())
}

fn trusted_key(pem: &str) -> Result<TrustedKey> {
    let spki = decode_pem_spki(pem)?;
    let sec1 = sec1_p256_public_key(&spki).context("invalid signing public key PEM")?;
    let keyid = digest(&SHA256, &spki);
    let keyid = keyid
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(TrustedKey {
        keyid,
        key: UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, sec1),
    })
}

/// The committed public key is the only trust root. A test build may substitute
/// one PEM. Previous-key files are not read.
fn trusted_keys() -> Result<Vec<TrustedKey>> {
    #[cfg(debug_assertions)]
    if let Ok(pem) = std::env::var("JMODGUARD_TEST_UPDATE_SIGNING_PUBLIC_PEM")
        && !pem.is_empty()
    {
        return Ok(vec![trusted_key(&pem)?]);
    }
    Ok(vec![trusted_key(CURRENT_PUBLIC_PEM)?])
}

fn docker_reference(component: &str) -> Result<&'static str> {
    match component {
        "bot" => Ok("ghcr.io/jatmn/jmodguard-bot"),
        "api" => Ok("ghcr.io/jatmn/jmodguard-api"),
        "webui" => Ok("ghcr.io/jatmn/jmodguard-webui"),
        _ => bail!("unsupported update component"),
    }
}

pub fn signature_tag_for_digest(manifest_digest: &str) -> Result<String> {
    ensure!(
        manifest_digest.len() == 71 && manifest_digest.starts_with("sha256:"),
        "invalid manifest digest"
    );
    Ok(format!(
        "sha256-{}.sig",
        manifest_digest.strip_prefix("sha256:").unwrap()
    ))
}

pub fn envelope_from_cosign_payload(payload: &[u8], signature_b64: &str) -> Result<Vec<u8>> {
    let signature_bytes = STANDARD
        .decode(signature_b64)
        .map_err(|_| ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })?;
    let keys = trusted_keys().map_err(|_| ImageSignatureRejection {
        phrase: BAD_IMAGE_SIGNATURE,
    })?;
    // The Cosign annotation has no key id. A signer exists only when a pinned
    // key verifies these bytes. Failure here is a bad signature, not an
    // untrusted key.
    let Some(keyid) = keys.iter().find_map(|key| {
        key.key
            .verify(payload, &signature_bytes)
            .is_ok()
            .then(|| key.keyid.clone())
    }) else {
        return Err(ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
        .into());
    };
    let envelope = serde_json::json!({
        "payloadType": PAYLOAD_TYPE,
        "payload": STANDARD.encode(payload),
        "signatures": [{
            "keyid": keyid,
            "sig": signature_b64
        }]
    });
    serde_json::to_vec(&envelope).map_err(|_| {
        anyhow::Error::new(ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })
    })
}

pub fn verify_manifest_signature(
    component: &str,
    manifest_digest: &str,
    manifest_bytes: &[u8],
    signature_body: &[u8],
    now: DateTime<Utc>,
    expiry: SignatureExpiry,
) -> Result<()> {
    let envelope: Envelope =
        serde_json::from_slice(signature_body).map_err(|_| ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })?;
    ensure!(
        envelope.payload_type == PAYLOAD_TYPE,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    ensure!(
        envelope.signatures.len() == 1,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    let entry = &envelope.signatures[0];
    let payload_bytes =
        STANDARD
            .decode(&envelope.payload)
            .map_err(|_| ImageSignatureRejection {
                phrase: BAD_IMAGE_SIGNATURE,
            })?;
    let signature_bytes = STANDARD
        .decode(&entry.sig)
        .map_err(|_| ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })?;
    let keys = trusted_keys().map_err(|_| ImageSignatureRejection {
        phrase: BAD_IMAGE_SIGNATURE,
    })?;
    let Some(trusted) = keys.iter().find(|key| key.keyid == entry.keyid) else {
        let phrase = if entry.keyid.is_empty() {
            BAD_IMAGE_SIGNATURE
        } else {
            UNTRUSTED_IMAGE_SIGNATURE_KEY
        };
        return Err(ImageSignatureRejection { phrase }.into());
    };
    trusted
        .key
        .verify(&payload_bytes, &signature_bytes)
        .map_err(|_| ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })?;
    let payload: SimpleSigningPayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        })?;
    ensure!(
        payload.critical.kind == SIGNATURE_TYPE,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    ensure!(
        payload.critical.identity.docker_reference == docker_reference(component)?,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    ensure!(
        payload.critical.image.docker_manifest_digest == manifest_digest,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    let actual = format!("sha256:{}", crate::oci::sha256_hex(manifest_bytes));
    ensure!(
        actual == manifest_digest,
        ImageSignatureRejection {
            phrase: BAD_IMAGE_SIGNATURE,
        }
    );
    let not_after = payload
        .optional
        .as_ref()
        .and_then(|map| map.get("io.jmodguard.not-after"))
        .and_then(|value| value.as_str())
        .ok_or(ImageSignatureRejection {
            phrase: EXPIRED_IMAGE_SIGNATURE,
        })?;
    let expires = DateTime::parse_from_rfc3339(not_after)
        .map_err(|_| ImageSignatureRejection {
            phrase: EXPIRED_IMAGE_SIGNATURE,
        })?
        .with_timezone(&Utc);
    if expiry == SignatureExpiry::Enforce {
        ensure!(
            now < expires,
            ImageSignatureRejection {
                phrase: EXPIRED_IMAGE_SIGNATURE,
            }
        );
    }
    Ok(())
}

pub fn signature_rejection_phrase(error: &anyhow::Error) -> Option<&'static str> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<ImageSignatureRejection>()
            .map(|rejection| rejection.phrase)
    })
}

pub mod test_support {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::json;

    fn p256_spki(sec1: &[u8]) -> Vec<u8> {
        const PREFIX: &[u8] = &[
            0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
            0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
        ];
        let mut der = PREFIX.to_vec();
        der.extend_from_slice(sec1);
        der
    }

    pub fn generate_key_pair() -> (String, String) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap();
        let sec1 = key_pair.public_key().as_ref();
        assert_eq!(sec1.len(), 65, "P-256 public point");
        let public_pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            STANDARD.encode(p256_spki(sec1))
        );
        let private_pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(pkcs8.as_ref())
        );
        (public_pem, private_pem)
    }

    fn signing_key(private_pem: &str) -> EcdsaKeyPair {
        let rng = SystemRandom::new();
        let pkcs8 = STANDARD
            .decode(
                private_pem
                    .lines()
                    .filter(|line| !line.starts_with("-----"))
                    .collect::<String>(),
            )
            .unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng).unwrap()
    }

    fn payload(component: &str, manifest_digest: &str, not_after: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "critical": {
                "identity": {"docker-reference": docker_reference(component).unwrap()},
                "image": {"docker-manifest-digest": manifest_digest},
                "type": SIGNATURE_TYPE
            },
            "optional": {"io.jmodguard.not-after": not_after}
        }))
        .unwrap()
    }

    /// OCI manifest Cosign stores at the `.sig` tag, plus the config and payload blobs.
    pub fn sign_manifest_cosign(
        private_pem: &str,
        _public_pem: &str,
        component: &str,
        manifest_digest: &str,
        not_after: &str,
    ) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
        let rng = SystemRandom::new();
        let key_pair = signing_key(private_pem);
        let payload = payload(component, manifest_digest, not_after);
        let signature = key_pair.sign(&rng, &payload).unwrap();
        let sig_b64 = STANDARD.encode(signature.as_ref());
        let config = b"{}";
        let config_digest = format!("sha256:{}", crate::oci::sha256_hex(config));
        let payload_digest = format!("sha256:{}", crate::oci::sha256_hex(&payload));
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config.len()
            },
            "layers": [{
                "mediaType": "application/vnd.dev.cosign.simplesigning.v1+json",
                "digest": payload_digest,
                "size": payload.len(),
                "annotations": {
                    "dev.cosignproject.cosign/signature": sig_b64
                }
            }]
        }))
        .unwrap();
        (
            manifest,
            vec![(config_digest, config.to_vec()), (payload_digest, payload)],
        )
    }

    pub fn sign_manifest(
        private_pem: &str,
        public_pem: &str,
        component: &str,
        manifest_digest: &str,
        not_after: &str,
    ) -> Vec<u8> {
        let rng = SystemRandom::new();
        let key_pair = signing_key(private_pem);
        let trusted = trusted_key(public_pem).unwrap();
        let payload = payload(component, manifest_digest, not_after);
        let signature = key_pair.sign(&rng, &payload).unwrap();
        serde_json::to_vec(&json!({
            "payloadType": PAYLOAD_TYPE,
            "payload": STANDARD.encode(&payload),
            "signatures": [{
                "keyid": trusted.keyid,
                "sig": STANDARD.encode(signature.as_ref())
            }]
        }))
        .unwrap()
    }

    /// Flip one byte of the Cosign layer annotation. The payload and key stay
    /// those of the pinned signer; the annotation itself carries no key id.
    pub fn tamper_cosign_annotation(manifest: &[u8]) -> Vec<u8> {
        let mut value: serde_json::Value = serde_json::from_slice(manifest).unwrap();
        let sig = value["layers"][0]["annotations"]["dev.cosignproject.cosign/signature"]
            .as_str()
            .unwrap();
        let mut raw = STANDARD.decode(sig).unwrap();
        raw[0] ^= 0xff;
        value["layers"][0]["annotations"]["dev.cosignproject.cosign/signature"] =
            json!(STANDARD.encode(raw));
        serde_json::to_vec(&value).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_public_key_is_spki_not_a_raw_point() {
        let der = decode_pem_spki(CURRENT_PUBLIC_PEM).unwrap();
        let sec1 = sec1_p256_public_key(&der).unwrap();
        assert!(der.len() > 65);
        assert_eq!(sec1.len(), 65);
        assert_eq!(sec1[0], 0x04);
        let key = trusted_key(CURRENT_PUBLIC_PEM).unwrap();
        assert_eq!(key.keyid.len(), 64);
    }

    #[test]
    fn missing_signer_is_a_bad_signature() {
        let manifest = b"manifest-bytes";
        let digest = format!("sha256:{}", crate::oci::sha256_hex(manifest));
        let envelope = serde_json::json!({
            "payloadType": PAYLOAD_TYPE,
            "payload": STANDARD.encode(b"{}"),
            "signatures": [{"keyid": "", "sig": STANDARD.encode([1u8; 64])}]
        });
        let error = verify_manifest_signature(
            "bot",
            &digest,
            manifest,
            &serde_json::to_vec(&envelope).unwrap(),
            Utc::now(),
            SignatureExpiry::Enforce,
        )
        .unwrap_err();
        assert_eq!(
            signature_rejection_phrase(&error),
            Some(BAD_IMAGE_SIGNATURE)
        );
    }
}
