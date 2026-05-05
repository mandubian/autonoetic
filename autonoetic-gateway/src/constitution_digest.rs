//! Canonical constitution digest utilities (R+++2).
//!
//! Digest input is intentionally explicit and deterministic:
//! - full constitution text
//! - right-id -> enforcement citation table
//! - rule-id -> enforcement citation table
//!
//! Runtime wiring:
//! - paths come from `GatewayConfig.constitution`
//! - `initialize_constitution()` must run before read APIs
//! - startup refuses boot if lock integrity checks fail.

use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionCanonicalization {
    pub algorithm: String,
    pub payload: String,
    pub rules_prefix: String,
    pub rights_prefix: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionLockSignature {
    pub algorithm: String,
    pub signer_id: String,
    pub signature_b64: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionLock {
    pub format_version: u32,
    pub constitution_id: String,
    pub constitution_version: String,
    pub constitution_source: String,
    pub constitution_digest: String,
    pub rule_enforcement_count: usize,
    pub right_enforcement_count: usize,
    pub canonicalization: ConstitutionCanonicalization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ConstitutionLockSignature>,
}

#[derive(Debug)]
struct ConstitutionRuntime {
    source_path: PathBuf,
    lock_path: PathBuf,
    gateway_dir: PathBuf,
    configured_source_label: String,
    text: String,
    digest: String,
    rights_enforcement: BTreeMap<String, String>,
    rules_enforcement: BTreeMap<String, String>,
    require_signature: bool,
    trusted_signers: HashMap<String, String>,
    lock: ConstitutionLock,
}

impl ConstitutionRuntime {
    fn load(config: &GatewayConfig) -> anyhow::Result<Self> {
        let configured_source_label = normalize_config_path_label(&config.constitution.source_path);
        let source_path = resolve_constitution_path(config, &config.constitution.source_path);
        let lock_path = resolve_constitution_path(config, &config.constitution.lock_path);

        let text = std::fs::read_to_string(&source_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read constitution source '{}': {}",
                source_path.display(),
                e
            )
        })?;
        let lock_json = std::fs::read_to_string(&lock_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read constitution lock '{}': {}",
                lock_path.display(),
                e
            )
        })?;
        let lock: ConstitutionLock = serde_json::from_str(&lock_json).map_err(|e| {
            anyhow::anyhow!(
                "constitution lock '{}' must be valid JSON: {}",
                lock_path.display(),
                e
            )
        })?;

        let rights_enforcement = extract_enforcement_table(&text, "Ri-");
        let rules_enforcement = extract_enforcement_table(&text, "R-");
        let payload = canonical_digest_payload(&text, &rights_enforcement, &rules_enforcement);
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        let digest = hex::encode(hasher.finalize());
        let gateway_dir = config.agents_dir.join(".gateway");

        Ok(Self {
            source_path,
            lock_path,
            gateway_dir,
            configured_source_label,
            text,
            digest,
            rights_enforcement,
            rules_enforcement,
            require_signature: config.constitution.require_signature,
            trusted_signers: config.constitution.trusted_signers.clone(),
            lock,
        })
    }
}

static RUNTIME: OnceLock<ConstitutionRuntime> = OnceLock::new();

pub fn initialize_constitution(config: &GatewayConfig) -> anyhow::Result<()> {
    let loaded = ConstitutionRuntime::load(config)?;
    if let Some(existing) = RUNTIME.get() {
        ensure_same_runtime_config(existing, &loaded)?;
        return Ok(());
    }
    match RUNTIME.set(loaded) {
        Ok(()) => Ok(()),
        Err(loaded) => {
            let existing = RUNTIME
                .get()
                .ok_or_else(|| anyhow::anyhow!("constitution runtime initialization race"))?;
            ensure_same_runtime_config(existing, &loaded)
        }
    }
}

fn ensure_same_runtime_config(
    existing: &ConstitutionRuntime,
    loaded: &ConstitutionRuntime,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        existing.source_path == loaded.source_path
            && existing.lock_path == loaded.lock_path
            && existing.gateway_dir == loaded.gateway_dir
            && existing.require_signature == loaded.require_signature
            && existing.trusted_signers == loaded.trusted_signers,
        "constitution runtime already initialized and cannot switch constitutional config in the same process (existing source='{}', lock='{}', gateway_dir='{}')",
        existing.source_path.display(),
        existing.lock_path.display(),
        existing.gateway_dir.display(),
    );
    Ok(())
}

fn runtime() -> &'static ConstitutionRuntime {
    RUNTIME.get().expect(
        "constitution runtime not initialized; call initialize_constitution(config) before accessing digest/profile APIs",
    )
}

pub fn constitution_text() -> &'static str {
    runtime().text.as_str()
}

pub fn constitution_lock() -> &'static ConstitutionLock {
    &runtime().lock
}

pub fn constitution_version() -> &'static str {
    constitution_lock().constitution_version.as_str()
}

pub fn constitution_format_version() -> u32 {
    constitution_lock().format_version
}

pub fn constitution_digest() -> &'static str {
    runtime().digest.as_str()
}

pub fn canonical_right_enforcement_table() -> BTreeMap<String, String> {
    runtime().rights_enforcement.clone()
}

pub fn canonical_rule_enforcement_table() -> BTreeMap<String, String> {
    runtime().rules_enforcement.clone()
}

pub fn canonical_constitution_profile() -> autonoetic_ofp::wire::ConstitutionProfile {
    autonoetic_ofp::wire::ConstitutionProfile {
        rules_enforcement: canonical_rule_enforcement_table(),
        rights_enforcement: canonical_right_enforcement_table(),
    }
}

pub fn verify_constitution_lock_integrity() -> anyhow::Result<()> {
    let rt = runtime();
    let lock = &rt.lock;
    anyhow::ensure!(
        lock.constitution_source == rt.configured_source_label,
        "constitution lock source mismatch (lock='{}', configured='{}')",
        lock.constitution_source,
        rt.configured_source_label
    );
    anyhow::ensure!(
        lock.constitution_digest == rt.digest,
        "constitution lock digest mismatch (lock={}, computed={})",
        lock.constitution_digest,
        rt.digest
    );
    anyhow::ensure!(
        lock.rule_enforcement_count == rt.rules_enforcement.len(),
        "constitution lock rule count mismatch (lock={}, computed={})",
        lock.rule_enforcement_count,
        rt.rules_enforcement.len()
    );
    anyhow::ensure!(
        lock.right_enforcement_count == rt.rights_enforcement.len(),
        "constitution lock right count mismatch (lock={}, computed={})",
        lock.right_enforcement_count,
        rt.rights_enforcement.len()
    );
    anyhow::ensure!(
        lock.canonicalization.algorithm == "sha256",
        "constitution lock canonicalization.algorithm must be 'sha256'"
    );
    anyhow::ensure!(
        lock.canonicalization.payload == "json({constitution_text,rights_enforcement,rules_enforcement})",
        "constitution lock canonicalization.payload must match the canonical digest payload declaration"
    );
    anyhow::ensure!(
        lock.canonicalization.rules_prefix == "R-",
        "constitution lock canonicalization.rules_prefix must be 'R-'"
    );
    anyhow::ensure!(
        lock.canonicalization.rights_prefix == "Ri-",
        "constitution lock canonicalization.rights_prefix must be 'Ri-'"
    );
    if let Some(signature) = lock.signature.as_ref() {
        anyhow::ensure!(
            signature.algorithm.eq_ignore_ascii_case("ed25519"),
            "constitution lock signature.algorithm must be 'ed25519'"
        );
        let payload = constitution_lock_signature_payload(lock)?;
        let public_key = resolve_constitution_signer_public_key(rt, &signature.signer_id)?;
        let verified = crate::runtime::crypto::verify_attestation_signature(
            &public_key,
            &payload,
            &signature.signature_b64,
        )?;
        anyhow::ensure!(
            verified,
            "constitution lock signature verification failed for signer '{}'",
            signature.signer_id
        );
    } else {
        anyhow::ensure!(
            !rt.require_signature,
            "constitution lock is unsigned but signature is required by config (constitution.require_signature=true)"
        );
    }
    Ok(())
}

pub fn constitution_lock_signature_payload(lock: &ConstitutionLock) -> anyhow::Result<Vec<u8>> {
    let payload = json!({
        "format_version": lock.format_version,
        "constitution_id": lock.constitution_id,
        "constitution_version": lock.constitution_version,
        "constitution_source": lock.constitution_source,
        "constitution_digest": lock.constitution_digest,
        "rule_enforcement_count": lock.rule_enforcement_count,
        "right_enforcement_count": lock.right_enforcement_count,
        "canonicalization": lock.canonicalization,
    });
    serde_json::to_vec(&payload)
        .map_err(|e| anyhow::anyhow!("failed to serialize constitution signature payload: {}", e))
}

fn resolve_constitution_signer_public_key(
    rt: &ConstitutionRuntime,
    signer_id: &str,
) -> anyhow::Result<[u8; 32]> {
    if let Some(fingerprint) = signer_id.strip_prefix("gateway:") {
        let public_path =
            rt.gateway_dir
                .join(crate::runtime::crypto::GatewayIdentityKey::PUBLIC_FILENAME);
        let bytes = std::fs::read(&public_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read gateway signer public key '{}': {}",
                public_path.display(),
                e
            )
        })?;
        anyhow::ensure!(
            bytes.len() == 32,
            "gateway signer public key '{}' has wrong length ({} bytes, expected 32)",
            public_path.display(),
            bytes.len()
        );
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        let actual = hex::encode(&out[..8]);
        anyhow::ensure!(
            fingerprint == actual,
            "gateway signer fingerprint mismatch (signer_id='{}', key fingerprint='{}')",
            signer_id,
            actual
        );
        return Ok(out);
    }

    let encoded = rt
        .trusted_signers
        .get(signer_id)
        .ok_or_else(|| anyhow::anyhow!("constitution signer '{}' is not trusted", signer_id))?;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|e| anyhow::anyhow!("invalid base64 public key for signer '{}': {}", signer_id, e))?;
    anyhow::ensure!(
        decoded.len() == 32,
        "trusted signer '{}' key has wrong length ({} bytes, expected 32)",
        signer_id,
        decoded.len()
    );
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(out)
}

fn canonical_digest_payload(
    constitution_text: &str,
    rights_enforcement: &BTreeMap<String, String>,
    rules_enforcement: &BTreeMap<String, String>,
) -> String {
    let payload = json!({
        "constitution_text": constitution_text,
        "rights_enforcement": rights_enforcement,
        "rules_enforcement": rules_enforcement,
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

fn extract_enforcement_table(text: &str, id_prefix: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        // Data rows in markdown tables always start with '|'.
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .map(str::to_string)
            .collect();
        // Target tables have 5 columns: ID | Rule/Right | Why/Source | Enforcement | Status.
        if cells.len() < 4 {
            continue;
        }
        let id = &cells[0];
        if !id.starts_with(id_prefix) {
            continue;
        }
        // Skip header/separator rows and keep only concrete IDs.
        if id == "ID" || id.starts_with("---") {
            continue;
        }
        let enforcement = cells[3].trim().to_string();
        if enforcement.is_empty() {
            continue;
        }
        out.insert(id.clone(), enforcement);
    }
    out
}

fn resolve_constitution_path(config: &GatewayConfig, configured_path: &Path) -> PathBuf {
    if configured_path.is_absolute() {
        return configured_path.to_path_buf();
    }
    let in_agents_dir = config.agents_dir.join(configured_path);
    if in_agents_dir.exists() {
        return in_agents_dir;
    }
    let primary_base = config
        .agents_dir
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let primary = primary_base.join(configured_path);
    if primary.exists() {
        return primary;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let secondary = cwd.join(configured_path);
        if secondary.exists() {
            return secondary;
        }
    }
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")));
    let tertiary = workspace_root.join(configured_path);
    if tertiary.exists() {
        return tertiary;
    }
    primary
}

fn normalize_config_path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_default_constitution() {
        initialize_constitution(&GatewayConfig::default())
            .expect("default constitution config should initialize");
    }

    #[test]
    fn constitution_digest_is_stable_hex_sha256() {
        init_default_constitution();
        let d = constitution_digest();
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, constitution_digest());
    }

    #[test]
    fn extracts_right_enforcement_rows() {
        init_default_constitution();
        let rights = canonical_right_enforcement_table();
        assert!(rights.contains_key("Ri-0.10"));
        assert!(rights
            .get("Ri-0.10")
            .expect("Ri-0.10 must exist")
            .contains("constitution_read"));
    }

    #[test]
    fn extracts_rule_enforcement_rows() {
        init_default_constitution();
        let rules = canonical_rule_enforcement_table();
        assert!(rules.contains_key("R-1.1"));
        assert!(rules
            .get("R-1.1")
            .expect("R-1.1 must exist")
            .contains("tool_call_processor"));
    }

    #[test]
    fn constitution_lock_matches_canonical_digest_and_counts() {
        init_default_constitution();
        verify_constitution_lock_integrity().expect("constitution lock integrity should hold");
    }

    #[test]
    fn constitution_lock_has_version_metadata() {
        init_default_constitution();
        let lock = constitution_lock();
        assert!(lock.format_version >= 1);
        assert!(!lock.constitution_id.trim().is_empty());
        assert!(!lock.constitution_version.trim().is_empty());
        assert!(
            lock.signature.is_some(),
            "default constitution lock should be signed"
        );
        assert_eq!(
            lock.signature
                .as_ref()
                .expect("signature should exist")
                .signer_id,
            "autonoetic:constitution:v1"
        );
        assert_eq!(
            lock.constitution_source,
            "docs/constitution/versions/2026.05.05/constitution.md"
        );
    }
}
