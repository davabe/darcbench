//! Ed25519 agent identity and bundle signatures.
//!
//! # Why Ed25519
//!
//! Small keys, small signatures, no parameter choices to get wrong, no RNG
//! required at signing time (deterministic nonces), and constant-time
//! implementations are the norm. For an artifact that must be verifiable by a
//! browser, a CLI and a server years from now, "no knobs" is the feature.
//!
//! # Key handling
//!
//! The agent's private key lives in a single file with mode `0600` under the
//! DARCBench state directory. It is generated on first use from the OS CSPRNG.
//! It is never transmitted, never logged, and never included in a bundle - only
//! the public key is.

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("could not read or create the agent key at {path}: {source}")]
    KeyIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("agent key file is malformed: expected {expected} bytes, found {found}")]
    MalformedKey { expected: usize, found: usize },
    #[error("operating system entropy is unavailable: {0}")]
    Entropy(String),
    #[error("signature is not valid for this payload")]
    BadSignature,
    #[error("public key is malformed")]
    BadPublicKey,
    #[error("canonicalisation failed: {0}")]
    Canonical(#[from] crate::canonical::CanonicalError),
}

/// A detached signature over canonical bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Always `ed25519`.
    pub algorithm: String,
    /// Canonicalisation used to produce the signed bytes; always `DCJ/1`.
    pub canonicalization: String,
    /// Base64 (standard alphabet, padded) of the 32-byte public key.
    pub public_key: String,
    /// Base64 of the 64-byte signature.
    pub value: String,
    /// Short, human-comparable fingerprint of the public key.
    pub key_id: String,
}

/// The agent's long-lived signing identity.
pub struct AgentKey {
    signing: SigningKey,
}

impl std::fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the private half, not even in a debug log.
        f.debug_struct("AgentKey")
            .field("key_id", &self.key_id())
            .finish_non_exhaustive()
    }
}

impl AgentKey {
    /// Generates a fresh key from the OS CSPRNG.
    pub fn generate() -> Result<Self, SigningError> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| SigningError::Entropy(e.to_string()))?;
        Ok(Self {
            signing: SigningKey::from_bytes(&seed),
        })
    }

    /// Loads the key at `path`, creating it with mode `0600` if absent.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, SigningError> {
        let io_err = |source: std::io::Error| SigningError::KeyIo {
            path: path.display().to_string(),
            source,
        };

        if path.exists() {
            let raw = std::fs::read(path).map_err(io_err)?;
            if raw.len() != 32 {
                return Err(SigningError::MalformedKey {
                    expected: 32,
                    found: raw.len(),
                });
            }
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&raw);
            return Ok(Self {
                signing: SigningKey::from_bytes(&seed),
            });
        }

        let key = Self::generate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        write_private(path, key.signing.as_bytes()).map_err(io_err)?;
        Ok(key)
    }

    pub fn public_key_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.signing.verifying_key().to_bytes())
    }

    /// Short fingerprint: first 8 bytes of SHA-256 over the public key, hex.
    pub fn key_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.signing.verifying_key().to_bytes());
        hex::encode(&digest[..8])
    }

    /// Signs the canonical form of `payload`.
    pub fn sign<T: Serialize>(&self, payload: &T) -> Result<Signature, SigningError> {
        let bytes = crate::canonical::canonical_json(payload)?;
        let signature = self.signing.sign(&bytes);
        Ok(Signature {
            algorithm: "ed25519".into(),
            canonicalization: crate::canonical::CANONICALIZATION.into(),
            public_key: self.public_key_b64(),
            value: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            key_id: self.key_id(),
        })
    }
}

/// Writes the private key with owner-only permissions.
///
/// The mode is set *at creation* rather than afterwards, so there is no window
/// in which the key exists world-readable.
#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Verifies a signature against the canonical form of `payload`.
///
/// Note what this proves and what it does not: it proves the holder of
/// `signature.public_key` signed exactly these bytes. Whether that key belongs
/// to a trustworthy agent is a separate question the control plane answers.
pub fn verify<T: Serialize>(payload: &T, signature: &Signature) -> Result<(), SigningError> {
    if signature.algorithm != "ed25519" {
        return Err(SigningError::BadSignature);
    }
    if signature.canonicalization != crate::canonical::CANONICALIZATION {
        return Err(SigningError::BadSignature);
    }

    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(&signature.public_key)
        .map_err(|_| SigningError::BadPublicKey)?;
    let key_array: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SigningError::BadPublicKey)?;
    let verifying = VerifyingKey::from_bytes(&key_array).map_err(|_| SigningError::BadPublicKey)?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(&signature.value)
        .map_err(|_| SigningError::BadSignature)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SigningError::BadSignature)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_array);

    let payload_bytes = crate::canonical::canonical_json(payload)?;
    verifying
        .verify(&payload_bytes, &sig)
        .map_err(|_| SigningError::BadSignature)
}

#[cfg(test)]
// In tests, `unwrap`/`expect` panicking *is* the failure signal.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]
mod tests {
    use super::*;

    #[derive(Serialize, Clone)]
    struct Payload {
        score: f64,
        run: String,
    }

    fn payload() -> Payload {
        Payload {
            score: 1234.5,
            run: "run_0123456789abcdef0123456789abcdef".into(),
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = AgentKey::generate().expect("keygen");
        let sig = key.sign(&payload()).expect("sign");
        assert_eq!(sig.algorithm, "ed25519");
        assert_eq!(sig.canonicalization, "DCJ/1");
        verify(&payload(), &sig).expect("verify");
    }

    #[test]
    fn tampering_with_any_field_invalidates_the_signature() {
        let key = AgentKey::generate().expect("keygen");
        let sig = key.sign(&payload()).expect("sign");

        let mut tampered = payload();
        tampered.score = 9999.0;
        assert!(matches!(
            verify(&tampered, &sig),
            Err(SigningError::BadSignature)
        ));

        let mut renamed = payload();
        renamed.run = "run_ffffffffffffffffffffffffffffffff".into();
        assert!(matches!(
            verify(&renamed, &sig),
            Err(SigningError::BadSignature)
        ));
    }

    #[test]
    fn a_forged_signature_value_is_rejected() {
        let key = AgentKey::generate().expect("keygen");
        let mut sig = key.sign(&payload()).expect("sign");
        sig.value = base64::engine::general_purpose::STANDARD.encode([0u8; 64]);
        assert!(verify(&payload(), &sig).is_err());
    }

    #[test]
    fn a_different_key_does_not_verify() {
        let a = AgentKey::generate().expect("keygen");
        let b = AgentKey::generate().expect("keygen");
        let sig_from_a = a.sign(&payload()).expect("sign");
        let mut swapped = sig_from_a.clone();
        swapped.public_key = b.public_key_b64();
        assert!(verify(&payload(), &swapped).is_err());
    }

    #[test]
    fn downgrading_the_canonicalisation_is_rejected() {
        let key = AgentKey::generate().expect("keygen");
        let mut sig = key.sign(&payload()).expect("sign");
        sig.canonicalization = "raw".into();
        assert!(matches!(
            verify(&payload(), &sig),
            Err(SigningError::BadSignature)
        ));
    }

    #[test]
    fn malformed_public_key_is_rejected() {
        let key = AgentKey::generate().expect("keygen");
        let mut sig = key.sign(&payload()).expect("sign");
        sig.public_key = "not-base64!!".into();
        assert!(matches!(
            verify(&payload(), &sig),
            Err(SigningError::BadPublicKey)
        ));
    }

    #[test]
    fn key_ids_are_stable_and_distinct() {
        let key = AgentKey::generate().expect("keygen");
        assert_eq!(key.key_id(), key.key_id());
        assert_eq!(key.key_id().len(), 16);
        let other = AgentKey::generate().expect("keygen");
        assert_ne!(key.key_id(), other.key_id());
    }

    #[test]
    fn debug_never_prints_the_private_key() {
        let key = AgentKey::generate().expect("keygen");
        let rendered = format!("{key:?}");
        assert!(rendered.contains("key_id"));
        assert!(!rendered.contains(&hex::encode(key.signing.to_bytes())));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_key_is_owner_only_and_reloads_identically() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "darcbench-keytest-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = dir.join("agent.key");

        let created = AgentKey::load_or_create(&path).expect("create");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "agent key must not be readable by other users");

        let reloaded = AgentKey::load_or_create(&path).expect("reload");
        assert_eq!(created.key_id(), reloaded.key_id());
        assert_eq!(created.public_key_b64(), reloaded.public_key_b64());

        // A signature made before the reload must verify after it.
        let sig = created.sign(&payload()).expect("sign");
        verify(&payload(), &sig).expect("verify across reload");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_truncated_key_file_is_rejected_rather_than_padded() {
        let dir = std::env::temp_dir().join(format!(
            "darcbench-badkey-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(1)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("agent.key");
        std::fs::write(&path, [0u8; 7]).expect("write");
        assert!(matches!(
            AgentKey::load_or_create(&path),
            Err(SigningError::MalformedKey {
                expected: 32,
                found: 7
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
