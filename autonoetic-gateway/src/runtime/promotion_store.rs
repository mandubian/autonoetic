//! Content Promotion Registry.
//!
//! Tracks promotion status (evaluator/auditor validation) per artifact.
//! This is the authoritative source for whether an artifact has passed validation gates.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use autonoetic_types::promotion::{Finding, PromotionRecord, PromotionRole};

/// Thread-safe promotion registry mapping artifact IDs to promotion records.
pub struct PromotionStore {
    store_path: std::path::PathBuf,
    records: Arc<Mutex<HashMap<String, PromotionRecord>>>,
}

impl PromotionStore {
    fn clear_role_evidence(record: &mut PromotionRecord) {
        record.evaluator_id = None;
        record.evaluator_pass = false;
        record.evaluator_findings.clear();
        record.evaluator_timestamp = None;
        record.auditor_id = None;
        record.auditor_pass = false;
        record.auditor_findings.clear();
        record.auditor_timestamp = None;
        record.static_evaluator_id = None;
        record.static_evaluator_pass = false;
        record.static_evaluator_findings.clear();
        record.static_evaluator_timestamp = None;
        record.unit_test_runner_id = None;
        record.unit_test_runner_pass = false;
        record.unit_test_runner_findings.clear();
        record.unit_test_runner_timestamp = None;
        record.sealed_evaluator_id = None;
        record.sealed_evaluator_pass = false;
        record.sealed_evaluator_findings.clear();
        record.sealed_evaluator_timestamp = None;
    }

    /// Creates a new PromotionStore, loading existing records from disk.
    pub fn new(gateway_dir: &Path) -> anyhow::Result<Self> {
        let store_path = gateway_dir.join("promotion_registry.json");
        let records = if store_path.exists() {
            let json = std::fs::read_to_string(&store_path)?;
            let records: HashMap<String, PromotionRecord> = serde_json::from_str(&json)?;
            tracing::info!(
                target: "promotion_store",
                path = %store_path.display(),
                count = records.len(),
                "Loaded existing promotion registry"
            );
            records
        } else {
            HashMap::new()
        };

        Ok(Self {
            store_path,
            records: Arc::new(Mutex::new(records)),
        })
    }

    /// Records or updates a promotion record for an artifact.
    ///
    /// If a record already exists for this artifact, updates the role-specific fields.
    pub fn record_promotion(
        &self,
        artifact_id: String,
        artifact_digest: Option<String>,
        content_digest: Option<String>,
        role: PromotionRole,
        agent_id: &str,
        pass: bool,
        findings: Vec<Finding>,
        summary: Option<String>,
    ) -> anyhow::Result<PromotionRecord> {
        let timestamp = chrono::Utc::now().to_rfc3339();

        let mut records = self.records.lock().unwrap();

        let record = records
            .entry(artifact_id.clone())
            .or_insert_with(|| PromotionRecord {
                artifact_id: artifact_id.clone(),
                artifact_digest: artifact_digest.clone(),
                content_digest: content_digest.clone(),
                evaluator_id: None,
                evaluator_pass: false,
                evaluator_findings: vec![],
                evaluator_timestamp: None,
                auditor_id: None,
                auditor_pass: false,
                auditor_findings: vec![],
                auditor_timestamp: None,
                static_evaluator_id: None,
                static_evaluator_pass: false,
                static_evaluator_findings: vec![],
                static_evaluator_timestamp: None,
                unit_test_runner_id: None,
                unit_test_runner_pass: false,
                unit_test_runner_findings: vec![],
                unit_test_runner_timestamp: None,
                sealed_evaluator_id: None,
                sealed_evaluator_pass: false,
                sealed_evaluator_findings: vec![],
                sealed_evaluator_timestamp: None,
                promotion_gate_version: "2.1".to_string(),
            });

        if let Some(artifact_digest) = artifact_digest {
            record.artifact_digest = Some(artifact_digest);
        }

        if let Some(content_digest) = content_digest {
            let current = record.content_digest.as_deref();
            if current != Some(content_digest.as_str()) {
                // New digest means new review subject; drop previous role evidence to avoid
                // mixing evaluator/auditor outcomes across different revision contents.
                if current.is_some() {
                    Self::clear_role_evidence(record);
                }
                record.content_digest = Some(content_digest);
            }
        }

        let role_name = match role {
            PromotionRole::Evaluator => {
                record.evaluator_id = Some(agent_id.to_string());
                record.evaluator_pass = pass;
                record.evaluator_findings = findings;
                record.evaluator_timestamp = Some(timestamp);
                "evaluator"
            }
            PromotionRole::Auditor => {
                record.auditor_id = Some(agent_id.to_string());
                record.auditor_pass = pass;
                record.auditor_findings = findings;
                record.auditor_timestamp = Some(timestamp);
                "auditor"
            }
            PromotionRole::StaticEvaluator => {
                record.static_evaluator_id = Some(agent_id.to_string());
                record.static_evaluator_pass = pass;
                record.static_evaluator_findings = findings;
                record.static_evaluator_timestamp = Some(timestamp);
                "static_evaluator"
            }
            PromotionRole::UnitTestRunner => {
                record.unit_test_runner_id = Some(agent_id.to_string());
                record.unit_test_runner_pass = pass;
                record.unit_test_runner_findings = findings;
                record.unit_test_runner_timestamp = Some(timestamp);
                "unit_test_runner"
            }
            PromotionRole::SealedEvaluator => {
                record.sealed_evaluator_id = Some(agent_id.to_string());
                record.sealed_evaluator_pass = pass;
                record.sealed_evaluator_findings = findings;
                record.sealed_evaluator_timestamp = Some(timestamp);
                "sealed_evaluator"
            }
        };
        tracing::info!(
            target: "promotion_store",
            artifact_id = %artifact_id,
            agent_id = %agent_id,
            role = role_name,
            pass = pass,
            "Recorded promotion"
        );

        if let Some(summary) = summary {
            tracing::debug!(
                target: "promotion_store",
                artifact_id = %artifact_id,
                summary = %summary,
                "Promotion summary recorded"
            );
        }

        let record = record.clone();
        drop(records);

        self.save()?;

        Ok(record)
    }

    /// If a promotion record exists but has no bound content digest yet, attach one.
    pub fn bind_content_digest_if_unset(
        &self,
        artifact_id: &str,
        content_digest: &str,
    ) -> anyhow::Result<bool> {
        let mut records = self.records.lock().unwrap();
        let Some(record) = records.get_mut(artifact_id) else {
            return Ok(false);
        };
        if record.content_digest.is_none() {
            record.content_digest = Some(content_digest.to_string());
            drop(records);
            self.save()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Reconciles an existing promotion record with a canonical revision content digest.
    ///
    /// When the digest changes for the same artifact ID, prior evaluator/auditor evidence is
    /// cleared to prevent accidental evidence replay across revision variants.
    pub fn reconcile_content_digest_for_revision(
        &self,
        artifact_id: &str,
        content_digest: &str,
    ) -> anyhow::Result<bool> {
        let mut records = self.records.lock().unwrap();
        let Some(record) = records.get_mut(artifact_id) else {
            return Ok(false);
        };

        let current = record.content_digest.as_deref();
        if current == Some(content_digest) {
            return Ok(false);
        }

        if current.is_some() {
            Self::clear_role_evidence(record);
        }
        record.content_digest = Some(content_digest.to_string());
        drop(records);
        self.save()?;
        Ok(true)
    }

    /// Gets a promotion record by artifact ID.
    pub fn get_promotion(&self, artifact_id: &str) -> Option<PromotionRecord> {
        let records = self.records.lock().unwrap();
        records.get(artifact_id).cloned()
    }

    /// Lists all promotion records.
    pub fn list_promotions(&self) -> Vec<PromotionRecord> {
        let records = self.records.lock().unwrap();
        records.values().cloned().collect()
    }

    /// Returns true if an artifact has passed promotion for the given role.
    pub fn has_passed(&self, artifact_id: &str, role: &PromotionRole) -> bool {
        let records = self.records.lock().unwrap();
        if let Some(record) = records.get(artifact_id) {
            match role {
                PromotionRole::Evaluator => record.evaluator_pass,
                PromotionRole::Auditor => record.auditor_pass,
                PromotionRole::StaticEvaluator => record.static_evaluator_pass,
                PromotionRole::UnitTestRunner => record.unit_test_runner_pass,
                PromotionRole::SealedEvaluator => record.sealed_evaluator_pass,
            }
        } else {
            false
        }
    }

    /// Returns true if an artifact has passed both evaluator (or sealed evaluator) and auditor promotion.
    pub fn is_fully_promoted(&self, artifact_id: &str) -> bool {
        let records = self.records.lock().unwrap();
        if let Some(record) = records.get(artifact_id) {
            (record.evaluator_pass || record.sealed_evaluator_pass) && record.auditor_pass
        } else {
            false
        }
    }

    /// Saves the promotion registry to disk.
    fn save(&self) -> anyhow::Result<()> {
        let records = self.records.lock().unwrap();
        if let Some(parent) = self.store_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&*records)?;
        std::fs::write(&self.store_path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::promotion::FindingSeverity;
    use tempfile::tempdir;

    fn test_finding() -> Finding {
        Finding {
            severity: FindingSeverity::Info,
            description: "Test passed".to_string(),
            evidence: Some("Test output".to_string()),
        }
    }

    #[test]
    fn test_promotion_store_record_and_get() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        let artifact_id = "art_abc123".to_string();

        let record = store
            .record_promotion(
                artifact_id.clone(),
                Some("sha256:abc123".to_string()),
                Some("sha256:content-abc123".to_string()),
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![test_finding()],
                Some("All tests passed".to_string()),
            )
            .unwrap();

        assert_eq!(record.artifact_id, artifact_id);
        assert_eq!(
            record.content_digest.as_deref(),
            Some("sha256:content-abc123")
        );
        assert_eq!(record.evaluator_id, Some("evaluator.default".to_string()));
        assert!(record.evaluator_pass);
        assert_eq!(record.evaluator_findings.len(), 1);

        let retrieved = store.get_promotion(&artifact_id).unwrap();
        assert_eq!(retrieved.artifact_id, artifact_id);
        assert!(retrieved.evaluator_pass);
    }

    #[test]
    fn test_promotion_store_both_roles() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        let artifact_id = "art_both".to_string();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                None,
            )
            .unwrap();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![],
                None,
            )
            .unwrap();

        assert!(store.has_passed(&artifact_id, &PromotionRole::Evaluator));
        assert!(store.has_passed(&artifact_id, &PromotionRole::Auditor));
        assert!(store.is_fully_promoted(&artifact_id));
    }

    #[test]
    fn test_promotion_store_evaluator_fail() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        let artifact_id = "art_fail".to_string();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                false,
                vec![Finding {
                    severity: FindingSeverity::Error,
                    description: "Test failed".to_string(),
                    evidence: None,
                }],
                None,
            )
            .unwrap();

        assert!(!store.has_passed(&artifact_id, &PromotionRole::Evaluator));
        assert!(!store.is_fully_promoted(&artifact_id));
    }

    #[test]
    fn test_promotion_store_update_role() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        let artifact_id = "art_update".to_string();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                false,
                vec![],
                None,
            )
            .unwrap();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                None,
            )
            .unwrap();

        let record = store.get_promotion(&artifact_id).unwrap();
        assert!(record.evaluator_pass);
        assert_eq!(record.evaluator_id, Some("evaluator.default".to_string()));
    }

    #[test]
    fn test_promotion_store_persistence() {
        let temp = tempdir().unwrap();

        let artifact_id = "art_persist".to_string();

        {
            let store = PromotionStore::new(temp.path()).unwrap();
            store
                .record_promotion(
                    artifact_id.clone(),
                    None,
                    None,
                    PromotionRole::Evaluator,
                    "evaluator.default",
                    true,
                    vec![],
                    None,
                )
                .unwrap();
        }

        {
            let store = PromotionStore::new(temp.path()).unwrap();
            let record = store.get_promotion(&artifact_id).unwrap();
            assert!(record.evaluator_pass);
            assert_eq!(record.evaluator_id, Some("evaluator.default".to_string()));
        }
    }

    #[test]
    fn test_promotion_store_not_found() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        assert!(store.get_promotion("art_nonexistent").is_none());
        assert!(!store.has_passed("art_nonexistent", &PromotionRole::Evaluator));
        assert!(!store.is_fully_promoted("art_nonexistent"));
    }

    #[test]
    fn test_bind_content_digest_if_unset_preserves_evidence() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();
        let artifact_id = "art_bind".to_string();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                None,
            )
            .unwrap();
        store
            .record_promotion(
                artifact_id.clone(),
                None,
                None,
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![],
                None,
            )
            .unwrap();

        let changed = store
            .bind_content_digest_if_unset(&artifact_id, "sha256:bind")
            .unwrap();
        assert!(changed);

        let record = store.get_promotion(&artifact_id).unwrap();
        assert_eq!(record.content_digest.as_deref(), Some("sha256:bind"));
        assert!(record.evaluator_pass);
        assert!(record.auditor_pass);
    }

    #[test]
    fn test_reconcile_content_digest_resets_mismatched_evidence() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();
        let artifact_id = "art_reconcile".to_string();

        store
            .record_promotion(
                artifact_id.clone(),
                None,
                Some("sha256:old".to_string()),
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                None,
            )
            .unwrap();
        store
            .record_promotion(
                artifact_id.clone(),
                None,
                Some("sha256:old".to_string()),
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![],
                None,
            )
            .unwrap();

        let changed = store
            .reconcile_content_digest_for_revision(&artifact_id, "sha256:new")
            .unwrap();
        assert!(changed);

        let record = store.get_promotion(&artifact_id).unwrap();
        assert_eq!(record.content_digest.as_deref(), Some("sha256:new"));
        assert!(!record.evaluator_pass);
        assert!(!record.auditor_pass);
        assert!(record.evaluator_id.is_none());
        assert!(record.auditor_id.is_none());
        assert!(record.evaluator_findings.is_empty());
        assert!(record.auditor_findings.is_empty());
    }
}
