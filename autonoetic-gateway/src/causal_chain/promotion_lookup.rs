//! Promotion Lookup via Causal Chain.
//!
//! Verifies that promotion records actually exist in the causal chain
//! (tamper-evidence for promotion claims).
//!
//! Since #1278 the witness is lean: entries carry `payload_hash` +
//! `payload_ref`, not the payload. Matching on the artifact id works from the
//! entry's `target` alone; the verdict fields (`role`, `pass`) are resolved
//! from the content-addressed copy and checked against `payload_hash` before
//! they are trusted. Legacy (v1) entries with inline payloads keep verifying.

use crate::causal_chain::{read_all_entries_across_segments, resolve_entry_payload};
use autonoetic_types::causal_chain::CausalChainEntry;
use autonoetic_types::promotion::PromotionRole;
use std::path::{Path, PathBuf};

pub struct PromotionLookup {
    history_dir: PathBuf,
}

impl PromotionLookup {
    pub fn new(history_dir: PathBuf) -> Self {
        Self { history_dir }
    }

    pub fn history_dir(&self) -> &Path {
        &self.history_dir
    }

    /// The witness file whose sibling `payloads/` directory holds the
    /// content-addressed copies. Any path inside `history_dir` resolves to
    /// the same CAS, so the canonical file name is fine here even when the
    /// entries were read from rotated segments.
    fn cas_anchor(&self) -> PathBuf {
        self.history_dir.join("causal_chain.jsonl")
    }

    /// Resolve an entry's payload: inline for v1 entries, from the CAS with
    /// hash verification for lean (v2) entries.
    fn entry_payload(&self, entry: &CausalChainEntry) -> anyhow::Result<Option<serde_json::Value>> {
        resolve_entry_payload(&self.cas_anchor(), entry)
    }

    /// Finds all promotion.record causal chain entries for a given artifact id.
    pub fn find_promotion_entries(
        &self,
        artifact_id: &str,
    ) -> anyhow::Result<Vec<CausalChainEntry>> {
        let entries = read_all_entries_across_segments(&self.history_dir)?;
        let mut matching = Vec::new();

        for entry in entries {
            if entry.category == "tool" && entry.action == "promotion_record" {
                // The lean witness carries the artifact id in `target` —
                // answerable without touching the payload store.
                if entry.target.as_deref() == Some(artifact_id) {
                    matching.push(entry);
                    continue;
                }
                // Legacy shape (v1 inline payload, or a v2 entry written
                // without `target`): match on the recorded arguments.
                if let Some(payload) = self.entry_payload(&entry)? {
                    if let Some(args) = payload.get("arguments") {
                        if let Some(recorded_artifact_id) = args.get("artifact_id") {
                            if recorded_artifact_id.as_str() == Some(artifact_id) {
                                matching.push(entry);
                            }
                        }
                    }
                }
            }
        }

        Ok(matching)
    }

    /// Verifies that a successful promotion.record call exists in the causal chain
    /// for the given artifact id and role.
    ///
    /// The verdict is read from the resolved payload and only trusted after
    /// its bytes verify against the entry's `payload_hash` — a tampered or
    /// truncated payload store fails loudly instead of passing.
    pub fn verify_promotion(
        &self,
        artifact_id: &str,
        role: &PromotionRole,
    ) -> anyhow::Result<bool> {
        let entries = self.find_promotion_entries(artifact_id)?;
        let role_str = role.as_str();

        for entry in entries {
            if matches!(
                entry.status,
                autonoetic_types::causal_chain::EntryStatus::Success
            ) {
                let Some(payload) = self.entry_payload(&entry)? else {
                    continue;
                };
                if let Some(args) = payload.get("arguments") {
                    if let Some(entry_role) = args.get("role") {
                        if entry_role.as_str() == Some(role_str) {
                            if let Some(pass) = args.get("pass") {
                                if pass.as_bool() == Some(true) {
                                    return Ok(true);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(false)
    }

    /// Returns the agent_id that recorded the promotion for a given artifact id and role.
    pub fn get_recorder(
        &self,
        artifact_id: &str,
        role: &PromotionRole,
    ) -> anyhow::Result<Option<String>> {
        let entries = self.find_promotion_entries(artifact_id)?;
        let role_str = role.as_str();

        for entry in entries {
            if matches!(
                entry.status,
                autonoetic_types::causal_chain::EntryStatus::Success
            ) {
                let Some(payload) = self.entry_payload(&entry)? else {
                    continue;
                };
                if let Some(args) = payload.get("arguments") {
                    if let Some(entry_role) = args.get("role") {
                        if entry_role.as_str() == Some(role_str) {
                            return Ok(Some(entry.actor_id.clone()));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// The witness's actual job (#1278): re-derive every entry hash and
    /// prev-linkage across all segments, proving the promotion history has
    /// not been rewritten.
    pub fn verify_witness_chain(
        &self,
    ) -> anyhow::Result<crate::causal_chain::ChainVerification> {
        crate::causal_chain::verify_chain(&self.history_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_chain::CausalLogger;
    use crate::log_redaction::RedactedPayload;
    use autonoetic_types::causal_chain::default_enforced_rules;
    use tempfile::tempdir;

    fn log_promotion(logger: &CausalLogger, artifact_id: &str, pass: bool) {
        logger
            .log(
                "evaluator.default",
                "session-1",
                Some("turn-1"),
                1,
                "tool",
                "promotion_record",
                autonoetic_types::causal_chain::EntryStatus::Success,
                Some(artifact_id),
                &default_enforced_rules(),
                Some(RedactedPayload::from_redacted(serde_json::json!({
                    "arguments": {
                        "artifact_id": artifact_id,
                        "role": "evaluator",
                        "pass": pass
                    }
                }))),
            )
            .unwrap();
    }

    #[test]
    fn test_promotion_lookup_verify() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();

        let logger = CausalLogger::new(history_dir.join("causal_chain.jsonl")).unwrap();
        log_promotion(&logger, "art_abc123", true);

        let lookup = PromotionLookup::new(history_dir);
        let result = lookup
            .verify_promotion("art_abc123", &PromotionRole::Evaluator)
            .unwrap();

        assert!(result);
    }

    #[test]
    fn test_promotion_lookup_not_found() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();

        let logger = CausalLogger::new(history_dir.join("causal_chain.jsonl")).unwrap();
        log_promotion(&logger, "art_abc123", true);

        let lookup = PromotionLookup::new(history_dir);
        let result = lookup
            .verify_promotion("art_different", &PromotionRole::Evaluator)
            .unwrap();

        assert!(!result);
    }

    #[test]
    fn test_promotion_lookup_wrong_role() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();

        let logger = CausalLogger::new(history_dir.join("causal_chain.jsonl")).unwrap();
        log_promotion(&logger, "art_abc123", true);

        let lookup = PromotionLookup::new(history_dir);
        let result = lookup
            .verify_promotion("art_abc123", &PromotionRole::Auditor)
            .unwrap();

        assert!(!result);
    }

    #[test]
    fn lean_witness_verdict_resolves_from_cas_and_fails_loud_on_tamper() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();

        let logger = CausalLogger::new(history_dir.join("causal_chain.jsonl")).unwrap();
        log_promotion(&logger, "art_abc123", true);

        // The witness file itself must not embed the verdict payload.
        let raw = std::fs::read_to_string(history_dir.join("causal_chain.jsonl")).unwrap();
        assert!(!raw.contains("\"pass\""), "lean witness leaked payload: {raw}");

        let lookup = PromotionLookup::new(history_dir.clone());
        assert!(lookup.verify_promotion("art_abc123", &PromotionRole::Evaluator).unwrap());

        // Tamper with the content-addressed copy: verification must fail
        // loudly (hash mismatch), never silently pass — even when the
        // tampered bytes still "look like" a passing verdict.
        for entry in std::fs::read_dir(history_dir.join("payloads")).unwrap().flatten() {
            std::fs::write(
                entry.path(),
                r#"{"arguments":{"artifact_id":"art_abc123","role":"evaluator","pass":false}}"#,
            )
            .unwrap();
        }
        assert!(
            lookup
                .verify_promotion("art_abc123", &PromotionRole::Evaluator)
                .is_err(),
            "tampered CAS bytes must not verify"
        );
    }

    #[test]
    fn verify_witness_chain_detects_rewritten_history() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();
        let log_path = history_dir.join("causal_chain.jsonl");

        let logger = CausalLogger::new(&log_path).unwrap();
        log_promotion(&logger, "art_ok", true);
        log_promotion(&logger, "art_two", true);

        let lookup = PromotionLookup::new(history_dir.clone());
        assert!(lookup.verify_witness_chain().unwrap().is_intact());

        // Mid-file rewrite (flip the first entry's artifact id): the stored
        // hashes go stale and verification must catch it.
        let raw = std::fs::read_to_string(&log_path).unwrap();
        std::fs::write(&log_path, raw.replace("art_ok", "art_evil")).unwrap();
        let verification = lookup.verify_witness_chain().unwrap();
        assert!(
            !verification.is_intact(),
            "rewritten witness history must fail verification"
        );
    }
}
