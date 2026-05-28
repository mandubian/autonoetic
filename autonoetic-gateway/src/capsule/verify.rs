//! Capsule digest + signature canonicalisation and verification.
//!
//! The capsule digest is SHA-256 over the **canonical JSON** of the manifest
//! with the `signature` field cleared to `None`. This makes signing
//! idempotent (signing twice produces the same input bytes) and lets the
//! verifier recompute the digest deterministically.
//!
//! `serde_json` emits keys in struct definition order for our concrete
//! `CapsuleManifest` (no `Value` round-trip), which is stable across builds
//! — the same canonicalisation choice the `state_attestation` module uses.

use anyhow::{Context, Result};
use autonoetic_types::capsule::{CapsuleManifest, CapsuleSignature};
use autonoetic_types::config::CapsuleConfig;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// Compute the canonical JSON bytes used for digest + signing. The
/// `signature` field is cleared in the copy before serialisation so that
/// a signed manifest's digest matches the unsigned one.
pub fn canonical_bytes(manifest: &CapsuleManifest) -> Result<Vec<u8>> {
    let mut canonical = manifest.clone();
    canonical.signature = None;
    serde_json::to_vec(&canonical)
        .context("serialising capsule manifest for canonicalisation")
}

/// SHA-256 hex digest over [`canonical_bytes`].
pub fn manifest_digest(manifest: &CapsuleManifest) -> Result<String> {
    let bytes = canonical_bytes(manifest)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Outcome of [`verify_signature`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// No signature present and `require_signature` was false.
    Absent,
    /// Signature present and verified successfully against the trusted-signer registry.
    Verified,
    /// Signature present but the `signer_id` is not in
    /// `CapsuleConfig::trusted_signers`.
    UntrustedSigner,
    /// Signature present but cryptographic verification failed.
    Mismatch,
    /// Signature present but malformed (wrong length, bad base64, etc.).
    Malformed,
    /// Signature absent and `require_signature` was true.
    MissingRequired,
}

impl SignatureStatus {
    /// Returns true when the status represents a successful trust decision
    /// (either no signature was required or the signature verified).
    pub fn is_ok(&self) -> bool {
        matches!(self, SignatureStatus::Absent | SignatureStatus::Verified)
    }
}

/// Verify `manifest.signature` against the provided trust store.
///
/// When `require_signature` is true and the manifest carries no
/// signature, the result is [`SignatureStatus::MissingRequired`].
pub fn verify_signature(
    manifest: &CapsuleManifest,
    config: &CapsuleConfig,
    require_signature: bool,
) -> Result<SignatureStatus> {
    let Some(sig) = manifest.signature.as_ref() else {
        return Ok(if require_signature {
            SignatureStatus::MissingRequired
        } else {
            SignatureStatus::Absent
        });
    };

    if sig.algorithm != "ed25519" {
        return Ok(SignatureStatus::Malformed);
    }
    let Some(pub_b64) = config.trusted_signers.get(&sig.signer_id) else {
        return Ok(SignatureStatus::UntrustedSigner);
    };
    let pub_bytes = match B64.decode(pub_b64) {
        Ok(b) if b.len() == 32 => b,
        _ => return Ok(SignatureStatus::Malformed),
    };
    let mut pub_arr = [0u8; 32];
    pub_arr.copy_from_slice(&pub_bytes);
    let vk = match VerifyingKey::from_bytes(&pub_arr) {
        Ok(v) => v,
        Err(_) => return Ok(SignatureStatus::Malformed),
    };
    let sig_bytes = match B64.decode(&sig.signature) {
        Ok(b) if b.len() == 64 => b,
        _ => return Ok(SignatureStatus::Malformed),
    };
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);

    let canonical = canonical_bytes(manifest)?;
    Ok(if vk.verify(&canonical, &signature).is_ok() {
        SignatureStatus::Verified
    } else {
        SignatureStatus::Mismatch
    })
}

/// Build a [`CapsuleSignature`] using the gateway's identity key. The
/// caller is responsible for setting `manifest.signature = Some(...)`
/// before persistence.
pub fn sign_manifest(
    manifest: &CapsuleManifest,
    key: &crate::runtime::crypto::GatewayIdentityKey,
) -> Result<CapsuleSignature> {
    let canonical = canonical_bytes(manifest)?;
    let signature = key.sign(&canonical);
    Ok(CapsuleSignature {
        algorithm: "ed25519".to_string(),
        signer_id: format!("gateway:{}", key.fingerprint()),
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::capsule::{CapsuleMode, CapsuleProvenance};

    fn empty_manifest() -> CapsuleManifest {
        CapsuleManifest {
            capsule_id: "cap_test".to_string(),
            format_version: autonoetic_types::capsule::CAPSULE_FORMAT_VERSION.to_string(),
            mode: CapsuleMode::Thin,
            created_at: "2026-05-28T00:00:00Z".to_string(),
            agent_id: "a".to_string(),
            revision_id: "rev".to_string(),
            revision_short_id: "rv".to_string(),
            content_digest: "d".to_string(),
            entrypoint: "SKILL.md".to_string(),
            runtime_lock: "runtime.lock".to_string(),
            included_artifacts: vec![],
            included_layers: vec![],
            included_skills: vec![],
            gateway_runtime: None,
            memory_snapshot: None,
            checkpoint_handle: None,
            redactions: vec![],
            signature: None,
            provenance: CapsuleProvenance {
                origin_node_id: "n".to_string(),
                gateway_version: "v".to_string(),
                trust_domain: "local".to_string(),
                parent_capsule_id: None,
            },
            requires_agents: vec![],
            requires_skills: vec![],
            scheduled_jobs: vec![],
            platform: None,
        }
    }

    #[test]
    fn canonical_bytes_ignores_signature_field() {
        let mut m1 = empty_manifest();
        let mut m2 = empty_manifest();
        m2.signature = Some(CapsuleSignature {
            algorithm: "ed25519".to_string(),
            signer_id: "gateway:abc".to_string(),
            signature: "AAAA".to_string(),
        });
        assert_eq!(canonical_bytes(&m1).unwrap(), canonical_bytes(&m2).unwrap());
        assert_eq!(manifest_digest(&m1).unwrap(), manifest_digest(&m2).unwrap());
        m1.agent_id = "different".to_string();
        assert_ne!(manifest_digest(&m1).unwrap(), manifest_digest(&m2).unwrap());
    }

    #[test]
    fn verify_absent_signature_returns_absent_or_missing() {
        let m = empty_manifest();
        let cfg = CapsuleConfig::default();
        assert_eq!(verify_signature(&m, &cfg, false).unwrap(), SignatureStatus::Absent);
        assert_eq!(
            verify_signature(&m, &cfg, true).unwrap(),
            SignatureStatus::MissingRequired
        );
    }

    #[test]
    fn untrusted_signer_returns_untrusted_signer() {
        let mut m = empty_manifest();
        m.signature = Some(CapsuleSignature {
            algorithm: "ed25519".to_string(),
            signer_id: "gateway:unknown".to_string(),
            signature: B64.encode([0u8; 64]),
        });
        let cfg = CapsuleConfig::default();
        assert_eq!(
            verify_signature(&m, &cfg, false).unwrap(),
            SignatureStatus::UntrustedSigner
        );
    }

    #[test]
    fn malformed_signature_returns_malformed() {
        let mut m = empty_manifest();
        m.signature = Some(CapsuleSignature {
            algorithm: "ed25519".to_string(),
            signer_id: "gateway:any".to_string(),
            signature: "not_base64!".to_string(),
        });
        let mut cfg = CapsuleConfig::default();
        cfg.trusted_signers
            .insert("gateway:any".to_string(), B64.encode([0u8; 32]));
        assert_eq!(
            verify_signature(&m, &cfg, false).unwrap(),
            SignatureStatus::Malformed
        );
    }

    #[test]
    fn sign_and_verify_roundtrip_with_gateway_key() {
        let temp = tempfile::tempdir().unwrap();
        let key = crate::runtime::crypto::GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let mut m = empty_manifest();
        let sig = sign_manifest(&m, &key).unwrap();
        m.signature = Some(sig);

        let mut cfg = CapsuleConfig::default();
        cfg.trusted_signers.insert(
            format!("gateway:{}", key.fingerprint()),
            B64.encode(key.public_key_bytes()),
        );
        assert_eq!(
            verify_signature(&m, &cfg, true).unwrap(),
            SignatureStatus::Verified
        );

        // Tamper with the manifest after signing — verification must fail.
        m.agent_id = "tampered".to_string();
        assert_eq!(
            verify_signature(&m, &cfg, true).unwrap(),
            SignatureStatus::Mismatch
        );
    }
}
