//! Vault for secure credential injection with encryption at rest.
//!
//! Secrets are encrypted using AES-256-GCM. A master key must be configured
//! via `AUTONOETIC_VAULT_KEY` (hex-encoded 32-byte key) or
//! `AUTONOETIC_VAULT_KEY_PATH` (path to file containing hex key).
//! Both persist and load require encryption — no plaintext fallback.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const VAULT_KEY_ENV: &str = "AUTONOETIC_VAULT_KEY";
const VAULT_KEY_PATH_ENV: &str = "AUTONOETIC_VAULT_KEY_PATH";

/// Encrypted vault file format.
#[derive(Debug, Serialize, Deserialize)]
struct EncryptedVault {
    /// File format id (`autonoetic-vault-enc`). On-disk vault is always this encrypted shape.
    format: String,
    version: u32,
    nonce: String,
    ciphertext: String,
}

/// Vault manages secrets for agents and injects them into tools safely.
#[derive(Debug)]
pub struct Vault {
    secrets: HashMap<String, SecretString>,
}

impl Vault {
    pub fn new() -> Self {
        Self {
            secrets: HashMap::new(),
        }
    }

    /// Load a secret from the environment or a secure keystore.
    pub fn load_secret(&mut self, key: &str, value: String) {
        self.secrets
            .insert(key.to_string(), SecretString::from(value));
    }

    /// Alias for explicit runtime secret writes.
    pub fn set_secret(&mut self, key: &str, value: String) {
        self.load_secret(key, value);
    }

    /// Retrieve a secret for secure injection (e.g., as an env var to a sandbox).
    ///
    /// The secret is wrapped in `SecretString` to prevent accidental logging.
    /// It must be explicitly exposed with `.expose_secret()` at the boundary.
    pub fn get_secret(&self, key: &str) -> Option<&SecretString> {
        self.secrets.get(key)
    }

    /// Clear all secrets from memory.
    pub fn clear(&mut self) {
        self.secrets.clear();
    }

    /// Get the configured master key, if any.
    /// Tries env var first, then key file.
    fn get_master_key() -> Option<[u8; 32]> {
        if let Ok(key_hex) = std::env::var(VAULT_KEY_ENV) {
            return hex_to_key(&key_hex);
        }
        if let Ok(key_path) = std::env::var(VAULT_KEY_PATH_ENV) {
            if let Ok(key_hex) = std::fs::read_to_string(&key_path) {
                return hex_to_key(key_hex.trim());
            }
        }
        None
    }

    /// Load a vault snapshot from disk. Expects an encrypted vault file.
    /// Fails if no master key is configured or the file is not a valid encrypted vault.
    pub fn load_from_file(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let raw = std::fs::read_to_string(path)?;
        let encrypted: EncryptedVault = serde_json::from_str(&raw)?;
        Self::decrypt_vault(&encrypted)
    }

    /// Persist current vault state to disk as an encrypted file.
    /// Requires a master key to be configured via env var or key file.
    pub fn persist_to_file(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let key = Self::get_master_key().ok_or_else(|| {
            anyhow::anyhow!(
                "Vault encryption requires {} or {} to be set",
                VAULT_KEY_ENV,
                VAULT_KEY_PATH_ENV
            )
        })?;
        let encrypted = self.encrypt_vault(&key)?;
        std::fs::write(path, serde_json::to_string_pretty(&encrypted)?)?;
        Ok(())
    }

    /// Encrypt the vault contents using AES-256-GCM.
    fn encrypt_vault(&self, key: &[u8; 32]) -> anyhow::Result<EncryptedVault> {
        let plain: HashMap<String, String> = self
            .secrets
            .iter()
            .map(|(k, v)| (k.clone(), v.expose_secret().to_string()))
            .collect();
        let plaintext = serde_json::to_string(&plain)?;

        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|e| anyhow::anyhow!("Failed to initialize AES-256-GCM cipher: {}", e))?;
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("Vault encryption failed: {}", e))?;

        Ok(EncryptedVault {
            format: "autonoetic-vault-enc".to_string(),
            version: 1,
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(ciphertext),
        })
    }

    /// Decrypt an encrypted vault file using AES-256-GCM.
    fn decrypt_vault(encrypted: &EncryptedVault) -> anyhow::Result<Self> {
        if encrypted.format != "autonoetic-vault-enc" {
            anyhow::bail!("Unrecognized encrypted vault format: {}", encrypted.format);
        }
        let key = Self::get_master_key()
            .ok_or_else(|| anyhow::anyhow!("Vault is encrypted but no master key is configured"))?;

        let nonce_bytes = hex::decode(&encrypted.nonce)?;
        if nonce_bytes.len() != 12 {
            anyhow::bail!(
                "Invalid nonce length in encrypted vault: expected 12, got {}",
                nonce_bytes.len()
            );
        }
        let ciphertext = hex::decode(&encrypted.ciphertext)?;

        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| anyhow::anyhow!("Failed to initialize AES-256-GCM cipher: {}", e))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = cipher
            .decrypt(nonce, ciphertext.as_ref())
            .map_err(|e| anyhow::anyhow!("Vault decryption failed: {}", e))?;

        let plain: HashMap<String, String> = serde_json::from_slice(&plaintext)?;
        let mut vault = Self::new();
        for (k, v) in plain {
            vault.set_secret(&k, v);
        }
        Ok(vault)
    }
}

/// Parse a hex-encoded 32-byte (64-char) key string into a [u8; 32] array.
fn hex_to_key(hex_str: &str) -> Option<[u8; 32]> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 64 {
        return None;
    }
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    Some(bytes.try_into().unwrap())
}

impl Default for Vault {
    fn default() -> Self {
        Self::new()
    }
}

/// Auto-generate and persist a vault encryption key if none is configured.
///
/// Writes a random 32-byte hex key to `{agents_dir}/.gateway/vault.key` and sets
/// `AUTONOETIC_VAULT_KEY_PATH` in the process environment so subsequent vault
/// operations pick it up without further configuration.
///
/// Returns immediately (no-op) if `AUTONOETIC_VAULT_KEY` or
/// `AUTONOETIC_VAULT_KEY_PATH` is already set.
pub fn ensure_default_key(agents_dir: &Path) -> anyhow::Result<()> {
    if std::env::var(VAULT_KEY_ENV).is_ok() || std::env::var(VAULT_KEY_PATH_ENV).is_ok() {
        return Ok(());
    }
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let key_path = gateway_dir.join("vault.key");
    if !key_path.exists() {
        let mut key_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key_bytes);
        let key_hex = hex::encode(key_bytes);
        std::fs::write(&key_path, &key_hex)?;
    }
    // Make the path available to the rest of the process.
    std::env::set_var(VAULT_KEY_PATH_ENV, &key_path);
    Ok(())
}

/// Return the default vault file path: `{agents_dir}/.gateway/vault.enc.json`.
pub fn default_vault_path(agents_dir: &Path) -> PathBuf {
    agents_dir.join(".gateway").join("vault.enc.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    fn test_hex_to_key_valid() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let key = hex_to_key(hex).expect("should parse");
        assert_eq!(key.len(), 32);
        assert_eq!(key[0], 0x00);
        assert_eq!(key[31], 0x1f);
    }

    #[test]
    fn test_hex_to_key_invalid_length() {
        assert!(hex_to_key("tooshort").is_none());
        assert!(
            hex_to_key("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e").is_none()
        );
    }

    #[test]
    fn test_hex_to_key_invalid_chars() {
        assert!(
            hex_to_key("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz")
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn test_vault_encrypted_roundtrip() {
        let key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        std::env::set_var(VAULT_KEY_ENV, key_hex);

        let temp = tempdir().unwrap();
        let path = temp.path().join("vault.json");

        let mut vault = Vault::new();
        vault.set_secret("API_KEY", "secret123".to_string());
        vault.persist_to_file(&path).unwrap();

        let loaded = Vault::load_from_file(&path).unwrap();
        assert_eq!(
            loaded.get_secret("API_KEY").unwrap().expose_secret(),
            "secret123"
        );

        std::env::remove_var(VAULT_KEY_ENV);
    }

    #[test]
    #[serial]
    fn test_vault_requires_key() {
        std::env::remove_var(VAULT_KEY_ENV);
        std::env::remove_var(VAULT_KEY_PATH_ENV);

        let temp = tempdir().unwrap();
        let path = temp.path().join("vault.enc.json");

        let vault = Vault::new();
        let result = vault.persist_to_file(&path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("encryption requires"));
    }

    #[test]
    #[serial]
    fn test_vault_key_from_file() {
        std::env::remove_var(VAULT_KEY_ENV);

        let key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let key_temp = tempdir().unwrap();
        let key_path = key_temp.path().join("master.key");
        std::fs::write(&key_path, key_hex).unwrap();
        std::env::set_var(VAULT_KEY_PATH_ENV, &key_path);

        let temp = tempdir().unwrap();
        let path = temp.path().join("vault.enc.json");

        let mut vault = Vault::new();
        vault.set_secret("API_KEY", "secret123".to_string());
        vault.persist_to_file(&path).unwrap();

        let loaded = Vault::load_from_file(&path).unwrap();
        assert_eq!(
            loaded.get_secret("API_KEY").unwrap().expose_secret(),
            "secret123"
        );

        std::env::remove_var(VAULT_KEY_PATH_ENV);
    }

    #[test]
    #[serial]
    fn test_vault_encrypted_nonce_is_unique() {
        let temp = tempdir().unwrap();
        let path1 = temp.path().join("vault1.enc.json");
        let path2 = temp.path().join("vault2.enc.json");

        let key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        std::env::set_var(VAULT_KEY_ENV, key_hex);

        let mut vault1 = Vault::new();
        vault1.set_secret("KEY", "value".to_string());
        vault1.persist_to_file(&path1).unwrap();

        let mut vault2 = Vault::new();
        vault2.set_secret("KEY", "value".to_string());
        vault2.persist_to_file(&path2).unwrap();

        let raw1 = std::fs::read_to_string(&path1).unwrap();
        let raw2 = std::fs::read_to_string(&path2).unwrap();

        // Same plaintext should produce different ciphertexts (different nonces)
        assert_ne!(raw1, raw2);

        std::env::remove_var(VAULT_KEY_ENV);
    }
}
