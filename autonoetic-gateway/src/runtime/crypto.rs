//! Cryptographic primitives.
//!
//! - `ManifestSigner` / `ManifestVerifier`: Ed25519 signatures on agent
//!   manifests to prevent tampering and knowledge poisoning.
//! - `GatewayIdentityKey`: per-gateway Ed25519 keypair persisted under the
//!   gateway directory, used to sign turn-boundary state attestations
//!   (R++1) and (later) to identify the gateway in federation handshakes
//!   (R+++2).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use std::path::{Path, PathBuf};

pub struct ManifestSigner {
    key: SigningKey,
}

impl ManifestSigner {
    /// Creates a signer from a 32-byte secret key.
    pub fn new(secret_bytes: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(secret_bytes),
        }
    }

    /// Generates a base64 encoded Ed25519 signature for the given content.
    pub fn sign(&self, content: &str) -> String {
        let signature = self.key.sign(content.as_bytes());
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD.encode(signature.to_bytes())
    }
}

pub struct ManifestVerifier;

impl ManifestVerifier {
    /// Verifies that the base64 signature matches the content and public key bytes.
    pub fn verify(
        public_bytes: &[u8; 32],
        content: &str,
        signature_b64: &str,
    ) -> anyhow::Result<bool> {
        let vk = VerifyingKey::from_bytes(public_bytes)
            .map_err(|e| anyhow::anyhow!("Invalid public key: {}", e))?;

        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let sig_bytes = STANDARD.decode(signature_b64)?;
        if sig_bytes.len() != 64 {
            return Ok(false);
        }

        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);

        Ok(vk.verify(content.as_bytes(), &signature).is_ok())
    }
}

/// Per-gateway Ed25519 identity key.
///
/// Persisted as a flat 32-byte file `state_attestation.ed25519` under the
/// gateway directory (alongside `gateway.db`). The public half is mirrored
/// to `state_attestation.ed25519.pub` so verifiers (and federation) can
/// read it cheaply without unlocking the private file.
///
/// On Unix, the private file's permissions are enforced to `0o600`. Reading
/// a key whose permissions are wider is rejected fail-shut — silent
/// tolerance of world-readable signing keys would defeat the whole point
/// of attestation.
#[derive(Debug)]
pub struct GatewayIdentityKey {
    signing_key: SigningKey,
    private_path: PathBuf,
}

impl GatewayIdentityKey {
    /// Filename for the private key inside the gateway directory.
    pub const PRIVATE_FILENAME: &'static str = "state_attestation.ed25519";
    /// Filename for the public-key sidecar.
    pub const PUBLIC_FILENAME: &'static str = "state_attestation.ed25519.pub";

    /// Load the identity key from the gateway directory, generating a fresh
    /// keypair on first start. The private file is created with mode `0o600`
    /// on Unix; subsequent loads verify the perms have not been widened.
    pub fn load_or_generate(gateway_dir: &Path) -> anyhow::Result<Self> {
        let private_path = gateway_dir.join(Self::PRIVATE_FILENAME);
        let public_path = gateway_dir.join(Self::PUBLIC_FILENAME);

        if private_path.exists() {
            check_private_permissions(&private_path)?;
            let bytes = std::fs::read(&private_path).map_err(|e| {
                anyhow::anyhow!(
                    "Cannot read gateway identity key at {}: {}",
                    private_path.display(),
                    e
                )
            })?;
            anyhow::ensure!(
                bytes.len() == 32,
                "Gateway identity key at {} has wrong length ({} bytes, expected 32). \
                 Refusing to operate on a malformed signing key.",
                private_path.display(),
                bytes.len()
            );
            let mut secret = [0u8; 32];
            secret.copy_from_slice(&bytes);
            let signing_key = SigningKey::from_bytes(&secret);
            // Repair the public sidecar if the private file existed but the
            // public file is missing (e.g. operator-created install).
            if !public_path.exists() {
                write_public_key_file(&public_path, &signing_key.verifying_key())?;
            }
            Ok(Self {
                signing_key,
                private_path,
            })
        } else {
            std::fs::create_dir_all(gateway_dir).map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create gateway directory {}: {}",
                    gateway_dir.display(),
                    e
                )
            })?;
            let mut secret = [0u8; 32];
            OsRng.fill_bytes(&mut secret);
            let signing_key = SigningKey::from_bytes(&secret);
            write_private_key_file(&private_path, &signing_key)?;
            write_public_key_file(&public_path, &signing_key.verifying_key())?;
            tracing::info!(
                target: "crypto",
                path = %private_path.display(),
                "Generated new gateway identity key (Ed25519, R++1 attestation)"
            );
            Ok(Self {
                signing_key,
                private_path,
            })
        }
    }

    /// Sign arbitrary content. Returns a base64-encoded signature.
    pub fn sign(&self, content: &[u8]) -> String {
        let signature = self.signing_key.sign(content);
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD.encode(signature.to_bytes())
    }

    /// Public verifying key bytes (32 bytes).
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// Short, hex-encoded fingerprint of the public key — first 8 bytes (16
    /// hex chars). Embedded in attestation blocks so a verifier can pin the
    /// key without serialising the full bytes inline every turn.
    pub fn fingerprint(&self) -> String {
        let bytes = self.public_key_bytes();
        hex::encode(&bytes[..8])
    }

    /// Path the private key was loaded from. Useful for diagnostics + tests.
    pub fn private_path(&self) -> &Path {
        &self.private_path
    }
}

fn write_private_key_file(path: &Path, key: &SigningKey) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create gateway identity key {}: {}",
                    path.display(),
                    e
                )
            })?;
        f.write_all(&key.to_bytes())?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Cannot create gateway identity key {}: {}",
                    path.display(),
                    e
                )
            })?;
        f.write_all(&key.to_bytes())?;
        f.sync_all().ok();
    }
    Ok(())
}

fn write_public_key_file(path: &Path, key: &VerifyingKey) -> anyhow::Result<()> {
    std::fs::write(path, key.to_bytes()).map_err(|e| {
        anyhow::anyhow!(
            "Cannot write gateway public key {}: {}",
            path.display(),
            e
        )
    })?;
    Ok(())
}

#[cfg(unix)]
fn check_private_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| {
        anyhow::anyhow!(
            "Cannot stat gateway identity key {}: {}",
            path.display(),
            e
        )
    })?;
    let mode = meta.permissions().mode() & 0o777;
    // Reject any permissions wider than 0o600 — group/world readability
    // defeats the point of a signing key.
    anyhow::ensure!(
        mode & 0o077 == 0,
        "Gateway identity key {} has overly permissive mode {:o}; expected 0o600. \
         Tighten with `chmod 600 {}` and restart, or delete the file to regenerate.",
        path.display(),
        mode,
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn check_private_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Verify a base64 signature produced by a `GatewayIdentityKey` against the
/// verifying key recorded in `state_attestation.ed25519.pub`.
pub fn verify_attestation_signature(
    public_bytes: &[u8; 32],
    content: &[u8],
    signature_b64: &str,
) -> anyhow::Result<bool> {
    let vk = VerifyingKey::from_bytes(public_bytes)
        .map_err(|e| anyhow::anyhow!("Invalid public key: {}", e))?;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let sig_bytes = STANDARD.decode(signature_b64)?;
    if sig_bytes.len() != 64 {
        return Ok(false);
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = Signature::from_bytes(&sig_arr);
    Ok(vk.verify(content, &signature).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let secret = [1u8; 32];
        let signer = ManifestSigner::new(&secret);

        let content = "The quick brown fox jumps over the lazy agent.";
        let sig_b64 = signer.sign(content);

        // Derive public key for verification
        let public_bytes = signer.key.verifying_key().to_bytes();

        assert!(ManifestVerifier::verify(&public_bytes, content, &sig_b64).unwrap());

        // Test tampering
        let tampered_content = "The quick brown fox jumps over the evil agent.";
        assert!(!ManifestVerifier::verify(&public_bytes, tampered_content, &sig_b64).unwrap());
    }

    #[test]
    fn gateway_identity_key_autogenerates_on_first_start() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).expect("first generation");
        let private_path = temp.path().join(GatewayIdentityKey::PRIVATE_FILENAME);
        let public_path = temp.path().join(GatewayIdentityKey::PUBLIC_FILENAME);
        assert!(private_path.exists());
        assert!(public_path.exists());
        let pub_bytes = std::fs::read(&public_path).unwrap();
        assert_eq!(pub_bytes.len(), 32);
        assert_eq!(key.public_key_bytes().to_vec(), pub_bytes);
        assert_eq!(key.fingerprint().len(), 16); // 8 bytes hex
    }

    #[test]
    fn gateway_identity_key_round_trip_load() {
        let temp = tempfile::tempdir().expect("tempdir");
        let k1 = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let fp1 = k1.fingerprint();
        drop(k1);
        // A second load on the same dir reuses the same key; the
        // fingerprint stays stable across restarts.
        let k2 = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        assert_eq!(k2.fingerprint(), fp1);
    }

    #[cfg(unix)]
    #[test]
    fn gateway_identity_key_rejects_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        // Generate first with strict perms.
        let _ = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let private_path = temp.path().join(GatewayIdentityKey::PRIVATE_FILENAME);
        // Widen permissions to simulate operator misconfiguration.
        let mut perm = std::fs::metadata(&private_path).unwrap().permissions();
        perm.set_mode(0o644);
        std::fs::set_permissions(&private_path, perm).unwrap();

        let err =
            GatewayIdentityKey::load_or_generate(temp.path()).expect_err("loose perms must fail");
        assert!(
            err.to_string().contains("overly permissive"),
            "{}",
            err
        );
    }

    #[test]
    fn signature_round_trip_via_verify_helper() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let payload = b"{\"agent_id\":\"a\",\"turn\":3}";
        let sig = key.sign(payload);
        let pub_bytes = key.public_key_bytes();
        assert!(verify_attestation_signature(&pub_bytes, payload, &sig).unwrap());
        // Mutated payload — signature must reject.
        assert!(
            !verify_attestation_signature(&pub_bytes, b"{\"agent_id\":\"b\",\"turn\":3}", &sig)
                .unwrap()
        );
    }
}
