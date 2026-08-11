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
        record.evaluator_execution_trace_id = None;
        record.auditor_id = None;
        record.auditor_pass = false;
        record.auditor_findings.clear();
        record.auditor_timestamp = None;
        record.static_evaluator_id = None;
        record.static_evaluator_pass = false;
        record.static_evaluator_findings.clear();
        record.static_evaluator_timestamp = None;
        record.static_evaluator_execution_trace_id = None;
        record.unit_test_runner_id = None;
        record.unit_test_runner_pass = false;
        record.unit_test_runner_findings.clear();
        record.unit_test_runner_timestamp = None;
        record.unit_test_runner_execution_trace_id = None;
        record.sealed_evaluator_id = None;
        record.sealed_evaluator_pass = false;
        record.sealed_evaluator_findings.clear();
        record.sealed_evaluator_timestamp = None;
        record.sealed_evaluator_execution_trace_id = None;
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
        execution_trace_id: Option<String>,
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
                evaluator_execution_trace_id: None,
                auditor_id: None,
                auditor_pass: false,
                auditor_findings: vec![],
                auditor_timestamp: None,
                static_evaluator_id: None,
                static_evaluator_pass: false,
                static_evaluator_findings: vec![],
                static_evaluator_timestamp: None,
                static_evaluator_execution_trace_id: None,
                unit_test_runner_id: None,
                unit_test_runner_pass: false,
                unit_test_runner_findings: vec![],
                unit_test_runner_timestamp: None,
                unit_test_runner_execution_trace_id: None,
                sealed_evaluator_id: None,
                sealed_evaluator_pass: false,
                sealed_evaluator_findings: vec![],
                sealed_evaluator_timestamp: None,
                sealed_evaluator_execution_trace_id: None,
                promotion_gate_version: "2.2".to_string(),
                blessed_packages: vec![],
                code_digest: None,
                contract_digest: None,
                prose_digest: None,
                carried_roles: std::collections::BTreeMap::new(),
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

        let role_name = role.as_str();

        match role {
            PromotionRole::Evaluator => {
                record.evaluator_id = Some(agent_id.to_string());
                record.evaluator_pass = pass;
                record.evaluator_findings = findings;
                record.evaluator_timestamp = Some(timestamp);
                record.evaluator_execution_trace_id = execution_trace_id;
            }
            PromotionRole::Auditor => {
                record.auditor_id = Some(agent_id.to_string());
                record.auditor_pass = pass;
                record.auditor_findings = findings;
                record.auditor_timestamp = Some(timestamp);
            }
            PromotionRole::StaticEvaluator => {
                record.static_evaluator_id = Some(agent_id.to_string());
                record.static_evaluator_pass = pass;
                record.static_evaluator_findings = findings;
                record.static_evaluator_timestamp = Some(timestamp);
                record.static_evaluator_execution_trace_id = execution_trace_id;
            }
            PromotionRole::UnitTestRunner => {
                record.unit_test_runner_id = Some(agent_id.to_string());
                record.unit_test_runner_pass = pass;
                record.unit_test_runner_findings = findings;
                record.unit_test_runner_timestamp = Some(timestamp);
                record.unit_test_runner_execution_trace_id = execution_trace_id;
            }
            PromotionRole::SealedEvaluator => {
                record.sealed_evaluator_id = Some(agent_id.to_string());
                record.sealed_evaluator_pass = pass;
                record.sealed_evaluator_findings = findings;
                record.sealed_evaluator_timestamp = Some(timestamp);
                record.sealed_evaluator_execution_trace_id = execution_trace_id;
            }
        }

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

    /// Bless the resolved dependency closure for a promoted artifact: freeze the
    /// versions the validated, approved run used (determinism inc 3). No-op if no
    /// record exists for the artifact. The set is typically derived from the
    /// artifact's layers via `LayerStore::aggregate_resolved_packages`.
    pub fn set_blessed_packages(
        &self,
        artifact_id: &str,
        packages: Vec<autonoetic_types::layer::ResolvedPackage>,
    ) -> anyhow::Result<bool> {
        let mut records = self.records.lock().unwrap();
        let Some(record) = records.get_mut(artifact_id) else {
            return Ok(false);
        };
        record.blessed_packages = packages;
        drop(records);
        self.save()?;
        Ok(true)
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
            record
                .get_role_result(role.as_str())
                .map(|(pass, _)| pass)
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Returns true if any federation-only role (StaticEvaluator,
    /// UnitTestRunner) has recorded a verdict for this artifact.
    /// SealedEvaluator is NOT included — it is the renamed legacy evaluator
    /// and is covered by the legacy Full gate, not the FullJury gate.
    pub fn has_federation_roles(&self, artifact_id: &str) -> bool {
        let records = self.records.lock().unwrap();
        if let Some(record) = records.get(artifact_id) {
            record.static_evaluator_id.is_some()
                || record.unit_test_runner_id.is_some()
        } else {
            false
        }
    }

    /// Returns the agent IDs of all federation roles that recorded a verdict,
    /// for distinct-identity enforcement (P-2.17 extension).
    /// Includes StaticEvaluator, UnitTestRunner, and SealedEvaluator (if present).
    pub fn federation_agent_ids(&self, artifact_id: &str) -> Vec<String> {
        let records = self.records.lock().unwrap();
        let mut ids = Vec::new();
        if let Some(record) = records.get(artifact_id) {
            if let Some(id) = &record.static_evaluator_id {
                ids.push(id.clone());
            }
            if let Some(id) = &record.unit_test_runner_id {
                ids.push(id.clone());
            }
            if let Some(id) = &record.sealed_evaluator_id {
                ids.push(id.clone());
            }
        }
        ids
    }

    /// Returns true if an artifact has passed both evaluator (or sealed evaluator) and auditor promotion.
    pub fn is_fully_promoted(&self, artifact_id: &str) -> bool {
        let records = self.records.lock().unwrap();
        if let Some(record) = records.get(artifact_id) {
            let eval_ok = record.evaluator_pass
                || record.sealed_evaluator_pass
                || record.static_evaluator_pass;
            let audit_ok = record.auditor_pass;
            let unit_test_ok = record
                .unit_test_runner_id
                .as_ref()
                .map(|_| record.unit_test_runner_pass)
                .unwrap_or(true);
            eval_ok && audit_ok && unit_test_ok
        } else {
            false
        }
    }

    /// Annotate an artifact's promotion record with the three federation
    /// carry-forward digests (Stage 1; see `docs/federation-carry-forward.md`,
    /// the design spec landing with #1067).
    ///
    /// Called by `promotion.record` after the verdict is written, copying the
    /// artifact's current digests onto the record so the verdict binds to the
    /// exact bytes the gate reviewed. No-op (returns `Ok(())`) if no record
    /// exists for the artifact yet — the digests will be attached on the next
    /// verdict recorded for it.
    pub fn set_federation_digests(
        &self,
        artifact_id: &str,
        code_digest: Option<String>,
        contract_digest: Option<String>,
        prose_digest: Option<String>,
    ) -> anyhow::Result<()> {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.get_mut(artifact_id) {
            record.code_digest = code_digest;
            record.contract_digest = contract_digest;
            record.prose_digest = prose_digest;
            drop(records);
            self.save()?;
        }
        Ok(())
    }

    /// Record a **carried-forward** verdict on `new_artifact_id`, copying the
    /// pass/findings from `prior` (which already recorded a pass in `role`)
    /// and attaching carry provenance. Called by `federation_escalate` only
    /// after `verify_carry_claim` has accepted the carry — never agent-supplied.
    ///
    /// The new artifact's record gets the role's verdict (same pass/findings
    /// as the prior), the current digests (copied so future carries can chain),
    /// and an entry in `carried_roles` so the operator and the FullJury gate
    /// can tell the verdict was carried rather than freshly run.
    pub fn record_carried_verdict(
        &self,
        new_artifact_id: &str,
        role: PromotionRole,
        prior: &PromotionRecord,
        provenance: autonoetic_types::promotion::RoleCarryProvenance,
        new_digests: (Option<String>, Option<String>, Option<String>),
    ) -> anyhow::Result<PromotionRecord> {
        // A carry is only sound if the prior artifact actually recorded a
        // verdict in this role — `get_role_result` returns a default for any
        // role name, so gate on `has_role_verdict` first.
        anyhow::ensure!(
            prior.has_role_verdict(role.as_str()),
            "prior record has no verdict in role '{}'",
            role.as_str()
        );
        let (prior_pass, prior_findings) = prior
            .get_role_result(role.as_str())
            .ok_or_else(|| anyhow::anyhow!(
                "prior record has no verdict in role '{}'", role.as_str()
            ))?;
        let original_agent_id = match role {
            PromotionRole::Evaluator => prior.evaluator_id.clone(),
            PromotionRole::Auditor => prior.auditor_id.clone(),
            PromotionRole::StaticEvaluator => prior.static_evaluator_id.clone(),
            PromotionRole::UnitTestRunner => prior.unit_test_runner_id.clone(),
            PromotionRole::SealedEvaluator => prior.sealed_evaluator_id.clone(),
        }
        .unwrap_or_else(|| role.as_str().to_string());
        let role_name = role.as_str().to_string();

        // Reuse record_promotion's field-writing for the role verdict, then
        // attach carry provenance + digests.
        let mut record = self.record_promotion(
            new_artifact_id.to_string(),
            prior.artifact_digest.clone(),
            prior.content_digest.clone(),
            role,
            &original_agent_id,
            prior_pass,
            prior_findings.to_vec(),
            None,
            None,
        )?;

        let mut records = self.records.lock().unwrap();
        if let Some(rec) = records.get_mut(new_artifact_id) {
            rec.carried_roles.insert(role_name, provenance);
            let (code, contract, prose) = new_digests;
            rec.code_digest = code.or_else(|| rec.code_digest.clone());
            rec.contract_digest = contract.or_else(|| rec.contract_digest.clone());
            rec.prose_digest = prose.or_else(|| rec.prose_digest.clone());
            record = rec.clone();
        }
        drop(records);
        self.save()?;
        Ok(record)
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
                Some("trace-eval-001".to_string()),
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
    fn blessed_packages_persist_and_no_op_when_missing() {
        use autonoetic_types::layer::ResolvedPackage;
        let temp = tempdir().unwrap();
        let artifact_id = "art_bless".to_string();

        let store = PromotionStore::new(temp.path()).unwrap();
        // No record yet → no-op (false), nothing to bless.
        assert!(!store
            .set_blessed_packages(&artifact_id, vec![])
            .unwrap());

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
                None,
            )
            .unwrap();
        let blessed = vec![ResolvedPackage {
            name: "requests".into(),
            version: "2.31.0".into(),
        }];
        assert!(store
            .set_blessed_packages(&artifact_id, blessed.clone())
            .unwrap());

        // Survives a reload from disk.
        let reloaded = PromotionStore::new(temp.path()).unwrap();
        assert_eq!(
            reloaded.get_promotion(&artifact_id).unwrap().blessed_packages,
            blessed
        );
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

    #[test]
    fn federation_digests_round_trip_via_setter_and_persist() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();
        let artifact_id = "art_digest1".to_string();

        // Record a verdict first — digests start None.
        store
            .record_promotion(
                artifact_id.clone(),
                Some("sha256:abc".to_string()),
                Some("sha256:content".to_string()),
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![test_finding()],
                None,
                None,
            )
            .unwrap();
        let before = store.get_promotion(&artifact_id).unwrap();
        assert!(before.code_digest.is_none());
        assert!(before.contract_digest.is_none());
        assert!(before.prose_digest.is_none());

        // Attach digests (as promotion.record does after recording).
        store
            .set_federation_digests(
                &artifact_id,
                Some("sha256:code-1".to_string()),
                Some("sha256:contract-1".to_string()),
                Some("sha256:prose-1".to_string()),
            )
            .unwrap();

        // Re-open the store to verify persistence (the JSON file round-trips
        // the new fields).
        let reopened = PromotionStore::new(temp.path()).unwrap();
        let after = reopened.get_promotion(&artifact_id).unwrap();
        assert_eq!(after.code_digest.as_deref(), Some("sha256:code-1"));
        assert_eq!(after.contract_digest.as_deref(), Some("sha256:contract-1"));
        assert_eq!(after.prose_digest.as_deref(), Some("sha256:prose-1"));
        // And the verdict itself is untouched.
        assert!(after.auditor_pass);
    }

    #[test]
    fn set_federation_digests_no_op_when_record_absent() {
        // If no verdict has been recorded for the artifact yet, the setter
        // must not panic and must not create an empty record.
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();
        store
            .set_federation_digests(
                "art_never_seen",
                Some("sha256:x".to_string()),
                None,
                None,
            )
            .unwrap();
        assert!(store.get_promotion("art_never_seen").is_none());
    }

    #[test]
    fn legacy_record_without_digest_fields_deserializes_as_none() {
        // A promotion_registry.json written before this feature has no
        // code_digest/contract_digest/prose_digest keys. It must deserialize
        // with None (not error), so existing deployments upgrade cleanly.
        // None digests = unverifiable under carry-forward = must re-run,
        // which is the intended fail-closed posture.
        let temp = tempdir().unwrap();
        let legacy_path = temp.path().join("promotion_registry.json");
        let legacy_json = r#"{
            "art_legacy1": {
                "artifact_id": "art_legacy1",
                "artifact_digest": "sha256:old",
                "content_digest": "sha256:old-content",
                "evaluator_id": "evaluator.default",
                "evaluator_pass": true,
                "evaluator_findings": [],
                "evaluator_timestamp": "2026-01-01T00:00:00Z",
                "auditor_id": null,
                "auditor_pass": false,
                "auditor_findings": [],
                "auditor_timestamp": null,
                "static_evaluator_id": null,
                "static_evaluator_pass": false,
                "static_evaluator_findings": [],
                "static_evaluator_timestamp": null,
                "unit_test_runner_id": null,
                "unit_test_runner_pass": false,
                "unit_test_runner_findings": [],
                "unit_test_runner_timestamp": null,
                "sealed_evaluator_id": null,
                "sealed_evaluator_pass": false,
                "sealed_evaluator_findings": [],
                "sealed_evaluator_timestamp": null,
                "promotion_gate_version": "2.2",
                "blessed_packages": []
            }
        }"#;
        std::fs::write(&legacy_path, legacy_json).unwrap();

        let store = PromotionStore::new(temp.path()).unwrap();
        let record = store.get_promotion("art_legacy1").unwrap();
        assert!(record.code_digest.is_none());
        assert!(record.contract_digest.is_none());
        assert!(record.prose_digest.is_none());
        assert!(record.evaluator_pass, "legacy verdict itself survives");
    }

    #[test]
    fn record_carried_verdict_copies_pass_and_attaches_provenance() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();

        // Prior artifact records a passing auditor verdict + digests.
        store
            .record_promotion(
                "art_prior".to_string(),
                Some("sha256:art-digest".to_string()),
                Some("sha256:content".to_string()),
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![test_finding()],
                Some("all clean".to_string()),
                None,
            )
            .unwrap();
        store
            .set_federation_digests(
                "art_prior",
                Some("sha256:code-x".to_string()),
                Some("sha256:contract-y".to_string()),
                Some("sha256:prose-prior".to_string()),
            )
            .unwrap();
        let prior = store.get_promotion("art_prior").unwrap();

        // Carry the auditor verdict onto the new artifact.
        let provenance = autonoetic_types::promotion::RoleCarryProvenance {
            prior_artifact_ref: "ar.prior123".to_string(),
            prior_artifact_id: "art_prior".to_string(),
            original_agent_id: "auditor.default".to_string(),
            verified_at: "2026-01-02T00:00:00Z".to_string(),
            prior_code_digest: Some("sha256:code-x".to_string()),
            prior_contract_digest: Some("sha256:contract-y".to_string()),
            justification: Some("prose-only fix".to_string()),
            strictness: Some("conservative".to_string()),
        };
        store
            .record_carried_verdict(
                "art_new",
                PromotionRole::Auditor,
                &prior,
                provenance,
                (
                    Some("sha256:code-x".to_string()),
                    Some("sha256:contract-y".to_string()),
                    Some("sha256:prose-new".to_string()),
                ),
            )
            .unwrap();

        let new_rec = store.get_promotion("art_new").unwrap();
        // The carried verdict: auditor pass copied, with the original agent id.
        assert!(new_rec.auditor_pass);
        assert_eq!(new_rec.auditor_id.as_deref(), Some("auditor.default"));
        // Digests are the current artifact's (so future carries can chain).
        assert_eq!(new_rec.code_digest.as_deref(), Some("sha256:code-x"));
        assert_eq!(new_rec.contract_digest.as_deref(), Some("sha256:contract-y"));
        assert_eq!(new_rec.prose_digest.as_deref(), Some("sha256:prose-new"));
        // Provenance: the carried_roles map names the prior artifact + role.
        let carry = new_rec.carried_roles.get("auditor").expect("provenance");
        assert_eq!(carry.prior_artifact_ref, "ar.prior123");
        assert_eq!(carry.prior_artifact_id, "art_prior");
        assert_eq!(carry.justification.as_deref(), Some("prose-only fix"));
        assert_eq!(carry.strictness.as_deref(), Some("conservative"));
    }

    #[test]
    fn record_carried_verdict_errors_when_prior_role_absent() {
        let temp = tempdir().unwrap();
        let store = PromotionStore::new(temp.path()).unwrap();
        // Prior artifact has an auditor verdict, but we try to carry unit_test_runner.
        store
            .record_promotion(
                "art_prior".to_string(),
                None,
                None,
                PromotionRole::Auditor,
                "auditor.default",
                true,
                vec![],
                None,
                None,
            )
            .unwrap();
        let prior = store.get_promotion("art_prior").unwrap();
        let provenance = autonoetic_types::promotion::RoleCarryProvenance {
            prior_artifact_ref: "ar.prior".to_string(),
            prior_artifact_id: "art_prior".to_string(),
            original_agent_id: "unit_test_runner.default".to_string(),
            verified_at: "2026-01-02T00:00:00Z".to_string(),
            prior_code_digest: None,
            prior_contract_digest: None,
            justification: None,
            strictness: None,
        };
        let result = store.record_carried_verdict(
            "art_new",
            PromotionRole::UnitTestRunner,
            &prior,
            provenance,
            (None, None, None),
        );
        assert!(result.is_err(), "carrying an absent role must fail");
    }
}
