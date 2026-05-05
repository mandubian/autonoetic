//! Canonical constitution digest utilities (R+++2 scaffolding).
//!
//! Digest input is intentionally explicit and deterministic:
//! - full constitution text
//! - right-id -> enforcement citation table
//! - rule-id -> enforcement citation table
//!
//! This module is used by `constitution_read` and `gateway.info` so all
//! digest surfaces stay aligned.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::OnceLock;

const CONSTITUTION_TEXT: &str = include_str!("../../docs/gateway-constitution.md");
const CONSTITUTION_LOCK_PATH: &str =
    "docs/constitution/versions/2026.05.05/gateway-constitution.lock.json";
const CONSTITUTION_LOCK_JSON: &str =
    include_str!("../../docs/constitution/versions/2026.05.05/gateway-constitution.lock.json");

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConstitutionCanonicalization {
    pub algorithm: String,
    pub payload: String,
    pub rules_prefix: String,
    pub rights_prefix: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConstitutionLock {
    pub format_version: u32,
    pub constitution_id: String,
    pub constitution_version: String,
    pub constitution_source: String,
    pub constitution_digest: String,
    pub rule_enforcement_count: usize,
    pub right_enforcement_count: usize,
    pub canonicalization: ConstitutionCanonicalization,
}

pub fn constitution_text() -> &'static str {
    CONSTITUTION_TEXT
}

pub fn constitution_lock() -> &'static ConstitutionLock {
    static LOCK: OnceLock<ConstitutionLock> = OnceLock::new();
    LOCK.get_or_init(|| {
        serde_json::from_str(CONSTITUTION_LOCK_JSON)
            .unwrap_or_else(|_| panic!("{CONSTITUTION_LOCK_PATH} must be valid JSON"))
    })
}

pub fn constitution_version() -> &'static str {
    constitution_lock().constitution_version.as_str()
}

pub fn constitution_format_version() -> u32 {
    constitution_lock().format_version
}

pub fn constitution_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| {
        let payload = canonical_digest_payload();
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        hex::encode(hasher.finalize())
    })
}

pub fn canonical_right_enforcement_table() -> BTreeMap<String, String> {
    extract_enforcement_table("Ri-")
}

pub fn canonical_rule_enforcement_table() -> BTreeMap<String, String> {
    // The canonical constitutional rule set is the numbered rule inventory
    // (R-<section>.<rule>) in §§1-11.
    extract_enforcement_table("R-")
}

pub fn canonical_constitution_profile() -> autonoetic_ofp::wire::ConstitutionProfile {
    autonoetic_ofp::wire::ConstitutionProfile {
        rules_enforcement: canonical_rule_enforcement_table(),
        rights_enforcement: canonical_right_enforcement_table(),
    }
}

pub fn verify_constitution_lock_integrity() -> anyhow::Result<()> {
    let lock = constitution_lock();
    let computed_digest = constitution_digest();
    anyhow::ensure!(
        lock.constitution_digest == computed_digest,
        "constitution lock digest mismatch (lock={}, computed={})",
        lock.constitution_digest,
        computed_digest
    );
    anyhow::ensure!(
        lock.rule_enforcement_count == canonical_rule_enforcement_table().len(),
        "constitution lock rule count mismatch (lock={}, computed={})",
        lock.rule_enforcement_count,
        canonical_rule_enforcement_table().len()
    );
    anyhow::ensure!(
        lock.right_enforcement_count == canonical_right_enforcement_table().len(),
        "constitution lock right count mismatch (lock={}, computed={})",
        lock.right_enforcement_count,
        canonical_right_enforcement_table().len()
    );
    anyhow::ensure!(
        lock.canonicalization.algorithm == "sha256",
        "constitution lock canonicalization.algorithm must be 'sha256'"
    );
    anyhow::ensure!(
        lock.canonicalization.rules_prefix == "R-",
        "constitution lock canonicalization.rules_prefix must be 'R-'"
    );
    anyhow::ensure!(
        lock.canonicalization.rights_prefix == "Ri-",
        "constitution lock canonicalization.rights_prefix must be 'Ri-'"
    );
    Ok(())
}

fn canonical_digest_payload() -> String {
    let payload = json!({
        "constitution_text": constitution_text(),
        "rights_enforcement": canonical_right_enforcement_table(),
        "rules_enforcement": canonical_rule_enforcement_table(),
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

fn extract_enforcement_table(id_prefix: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in constitution_text().lines() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constitution_digest_is_stable_hex_sha256() {
        let d = constitution_digest();
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, constitution_digest());
    }

    #[test]
    fn extracts_right_enforcement_rows() {
        let rights = canonical_right_enforcement_table();
        assert!(rights.contains_key("Ri-0.10"));
        assert!(rights
            .get("Ri-0.10")
            .expect("Ri-0.10 must exist")
            .contains("constitution_read"));
    }

    #[test]
    fn extracts_rule_enforcement_rows() {
        let rules = canonical_rule_enforcement_table();
        assert!(rules.contains_key("R-1.1"));
        assert!(rules
            .get("R-1.1")
            .expect("R-1.1 must exist")
            .contains("tool_call_processor"));
    }

    #[test]
    fn constitution_lock_matches_canonical_digest_and_counts() {
        verify_constitution_lock_integrity().expect("constitution lock integrity should hold");
    }

    #[test]
    fn constitution_lock_has_version_metadata() {
        let lock = constitution_lock();
        assert!(lock.format_version >= 1);
        assert!(!lock.constitution_id.trim().is_empty());
        assert!(!lock.constitution_version.trim().is_empty());
        assert_eq!(lock.constitution_source, "docs/gateway-constitution.md");
    }
}
