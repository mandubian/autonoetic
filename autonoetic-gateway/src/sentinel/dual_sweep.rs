//! Dual-sweep orchestrator: frozen baseline + current sentinel.
//!
//! Runs both the frozen baseline sentinel (Phase 1 only) and the current
//! sentinel (Phase 1 + Phase 2) in a single coordinated sweep, then:
//!
//! 1. **Annotates `baseline_agreed`** on current findings whose evidence
//!    anchors overlap with baseline findings, before the final DB persist.
//!    (`baseline_agreed` is immutable after insertion due to the append-only
//!    trigger, so annotation must happen pre-insert.)
//!
//! 2. **Records disagreements** in `security_sentinel_disagreements`:
//!    - `baseline_only` — the baseline found an anchor the current missed
//!      (possible sentinel regression or configuration drift).
//!    - `current_only` — the current sentinel's Phase-1 checks found an
//!      anchor the baseline missed (possible baseline staleness).
//!    Phase-2 (LLM-judgment) findings are excluded from disagreement
//!    comparison because the deterministic baseline has no Phase-2 layer.
//!
//! ## Recursive-trust note
//!
//! The baseline provides a stable reference independent of the current
//! sentinel. A regressed current sentinel cannot hide its own failure because
//! the baseline is frozen in the gateway image and cannot be modified by the
//! agent-promotion pipeline. Updating the baseline requires an explicit
//! operator CLI action with identity logging — not implemented in this module.

use anyhow::Result;
use autonoetic_types::security::{EvidenceAnchor, SecurityFinding};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::scheduler::gateway_store::{
    sentinel_disagreements::{DisagreementDirection, SentinelDisagreementRecord},
    GatewayStore,
};
use super::runner::{RawSweepFindings, SentinelRunner, SweepConfig, SweepResult};

/// Result of a dual sweep (baseline + current).
pub struct DualSweepResult {
    /// Findings from the baseline sentinel (Phase 1 only), persisted to DB.
    pub baseline: SweepResult,
    /// Findings from the current sentinel, persisted with `baseline_agreed` set.
    pub current: SweepResult,
    /// Number of current Phase-1 findings that the baseline also flagged.
    pub baseline_agreed_count: usize,
    /// Disagreements recorded in the DB this sweep.
    pub disagreements: Vec<SentinelDisagreementRecord>,
}

/// Orchestrates a dual sweep of the frozen baseline and the current sentinel.
pub struct DualSweepRunner {
    store: Arc<GatewayStore>,
    agents_dir: Option<PathBuf>,
}

impl DualSweepRunner {
    pub fn new(store: Arc<GatewayStore>) -> Self {
        Self {
            store,
            agents_dir: None,
        }
    }

    pub fn with_agents_dir(mut self, agents_dir: PathBuf) -> Self {
        self.agents_dir = Some(agents_dir);
        self
    }

    /// Run the dual sweep: baseline (Phase 1 only) then current (Phase 1 + 2).
    ///
    /// The `baseline_config` should have `sentinel_revision_id` set to the
    /// baseline sentinel revision (e.g. `"sentinel_baseline.frozen"`).
    /// The `current_config` is the main sentinel's config.
    pub fn run(
        &self,
        baseline_config: &SweepConfig,
        current_config: &SweepConfig,
    ) -> Result<DualSweepResult> {
        let sweep_at = chrono::Utc::now().to_rfc3339();

        // Enforce Phase-1-only on the baseline regardless of caller config.
        let baseline_config_p1 = SweepConfig {
            phase1_only: true,
            sentinel_revision_id: baseline_config.sentinel_revision_id.clone(),
            since_rfc3339: baseline_config.since_rfc3339.clone(),
            scan_limit: baseline_config.scan_limit,
            window_days: baseline_config.window_days,
            accretion_threshold: baseline_config.accretion_threshold,
            denial_threshold: baseline_config.denial_threshold,
            cluster_window_minutes: baseline_config.cluster_window_minutes,
            failure_burst_threshold: baseline_config.failure_burst_threshold,
            exec_repeat_threshold: baseline_config.exec_repeat_threshold,
            scope_agent_id: baseline_config.scope_agent_id.clone(),
        };

        // 1. Collect baseline findings (Phase 1 only, no prompt-injection scan).
        let baseline_runner = SentinelRunner::new(Arc::clone(&self.store));
        let baseline_raw = baseline_runner.collect_findings(&baseline_config_p1)?;

        // 2. Collect current findings (Phase 1 + 2).
        let mut current_runner = SentinelRunner::new(Arc::clone(&self.store));
        if let Some(ref dir) = self.agents_dir {
            current_runner = current_runner.with_agents_dir(dir.clone());
        }
        let mut current_raw = current_runner.collect_findings(current_config)?;

        // 3. Match baseline Phase-1 findings to current Phase-1 findings by anchor overlap.
        let (agreed_current_ids, disagreements) = compare_phase1(
            &baseline_raw,
            &current_raw,
            &sweep_at,
            &baseline_config_p1.sentinel_revision_id,
            &current_config.sentinel_revision_id,
        )?;

        // 4. Annotate baseline_agreed on current findings (before persisting).
        annotate_baseline_agreed(&mut current_raw, &agreed_current_ids);

        let baseline_agreed_count = agreed_current_ids.len();

        // 5. Persist baseline findings so baseline_finding_id references a real DB row.
        let baseline_result = baseline_runner.persist_findings(baseline_raw);

        // 6. Persist current findings (now annotated with baseline_agreed).
        let current_result = current_runner.persist_findings(current_raw);

        // 7. Persist disagreements.
        for d in &disagreements {
            if let Err(e) = self.store.insert_sentinel_disagreement(d) {
                tracing::warn!(
                    target: "sentinel.dual_sweep",
                    error = %e,
                    "Failed to persist sentinel disagreement {}",
                    d.disagreement_id
                );
            }
        }

        Ok(DualSweepResult {
            baseline: baseline_result,
            current: current_result,
            baseline_agreed_count,
            disagreements,
        })
    }
}

// ── Anchor matching ───────────────────────────────────────────────────────────

/// A stable key derived from an `EvidenceAnchor` for set membership tests.
fn anchor_key(anchor: &EvidenceAnchor) -> String {
    match anchor {
        EvidenceAnchor::CausalEvent { id } => format!("causal_event:{}", id),
        EvidenceAnchor::SkillMdDigest { value } => format!("skill_md_digest:{}", value),
        EvidenceAnchor::LayerDigest { value } => format!("layer_digest:{}", value),
        EvidenceAnchor::ArtifactId { id } => format!("artifact_id:{}", id),
        EvidenceAnchor::RevisionId { id } => format!("revision_id:{}", id),
        EvidenceAnchor::PromotionRecord { promotion_id } => {
            format!("promotion_record:{}", promotion_id)
        }
        EvidenceAnchor::SandboxEscapeRecord { rowid } => {
            format!("sandbox_escape_record:{}", rowid)
        }
        EvidenceAnchor::ApprovalRecord { request_id } => {
            format!("approval_record:{}", request_id)
        }
    }
}

/// Derive a key set for a finding (all its anchor keys).
fn finding_anchor_keys(f: &SecurityFinding) -> HashSet<String> {
    f.evidence_anchors.iter().map(anchor_key).collect()
}

// ── Comparison logic ──────────────────────────────────────────────────────────

/// Compare Phase-1 baseline and current findings.
///
/// Returns:
/// - A set of `finding_id`s in `current_raw` that the baseline also flagged.
/// - A vec of `SentinelDisagreementRecord`s to persist.
fn compare_phase1(
    baseline_raw: &RawSweepFindings,
    current_raw: &RawSweepFindings,
    sweep_at: &str,
    baseline_rev: &str,
    current_rev: &str,
) -> Result<(HashSet<String>, Vec<SentinelDisagreementRecord>)> {
    let baseline_p1: Vec<&SecurityFinding> = baseline_raw.all_phase1().collect();
    let current_p1: Vec<&SecurityFinding> = current_raw.all_phase1().collect();

    // Build an anchor_key → Vec<current_finding_id> index for O(n+m) matching.
    let mut current_by_anchor: HashMap<String, Vec<&SecurityFinding>> = HashMap::new();
    for cf in &current_p1 {
        for key in finding_anchor_keys(cf) {
            current_by_anchor.entry(key).or_default().push(cf);
        }
    }

    let mut agreed_current_ids: HashSet<String> = HashSet::new();
    // Track which current findings were matched (for current_only detection below).
    let mut matched_current_ids: HashSet<&str> = HashSet::new();
    let mut disagreements = Vec::new();

    for bf in &baseline_p1 {
        let mut found_match = false;
        for key in finding_anchor_keys(bf) {
            if let Some(matches) = current_by_anchor.get(&key) {
                for cf in matches {
                    agreed_current_ids.insert(cf.finding_id.clone());
                    matched_current_ids.insert(cf.finding_id.as_str());
                    found_match = true;
                }
            }
        }
        if !found_match {
            // Baseline flagged something current missed.
            let anchor_json = serde_json::to_string(&bf.evidence_anchors)
                .unwrap_or_else(|_| "[]".to_string());
            disagreements.push(SentinelDisagreementRecord {
                disagreement_id: format!("dis_{}", uuid::Uuid::new_v4()),
                sweep_at: sweep_at.to_string(),
                direction: DisagreementDirection::BaselineOnly,
                anchor_json,
                baseline_finding_id: Some(bf.finding_id.clone()),
                current_finding_id: None,
                baseline_sentinel_rev: baseline_rev.to_string(),
                current_sentinel_rev: current_rev.to_string(),
            });
        }
    }

    // For each current Phase-1 finding NOT matched by any baseline anchor, record current_only.
    for cf in &current_p1 {
        if !matched_current_ids.contains(cf.finding_id.as_str()) {
            let anchor_json = serde_json::to_string(&cf.evidence_anchors)
                .unwrap_or_else(|_| "[]".to_string());
            disagreements.push(SentinelDisagreementRecord {
                disagreement_id: format!("dis_{}", uuid::Uuid::new_v4()),
                sweep_at: sweep_at.to_string(),
                direction: DisagreementDirection::CurrentOnly,
                anchor_json,
                baseline_finding_id: None,
                current_finding_id: Some(cf.finding_id.clone()),
                baseline_sentinel_rev: baseline_rev.to_string(),
                current_sentinel_rev: current_rev.to_string(),
            });
        }
    }

    Ok((agreed_current_ids, disagreements))
}

/// Set `baseline_agreed = true` on current findings whose `finding_id` is in `agreed`.
fn annotate_baseline_agreed(current_raw: &mut RawSweepFindings, agreed: &HashSet<String>) {
    for f in current_raw
        .credential
        .iter_mut()
        .chain(current_raw.capability_accretion.iter_mut())
        .chain(current_raw.approval_bypass.iter_mut())
        .chain(current_raw.sandbox_escape.iter_mut())
        .chain(current_raw.supply_chain.iter_mut())
    {
        if agreed.contains(&f.finding_id) {
            f.baseline_agreed = true;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::security::{
        AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
        SecurityFinding,
    };

    fn make_finding(finding_type: FindingType, anchor: EvidenceAnchor) -> SecurityFinding {
        SecurityFinding::new(
            finding_type,
            FindingSeverity::Warning,
            0.8,
            Reproducibility::Deterministic,
            "remediate",
            "sentinel-rev-001",
        )
        .with_anchors(vec![anchor])
    }

    #[test]
    fn anchor_key_is_stable() {
        let a1 = EvidenceAnchor::CausalEvent { id: "evt_001".to_string() };
        let a2 = EvidenceAnchor::CausalEvent { id: "evt_001".to_string() };
        assert_eq!(anchor_key(&a1), anchor_key(&a2));
    }

    #[test]
    fn anchor_key_differs_by_type() {
        let a1 = EvidenceAnchor::RevisionId { id: "rev_001".to_string() };
        let a2 = EvidenceAnchor::SkillMdDigest { value: "rev_001".to_string() };
        assert_ne!(anchor_key(&a1), anchor_key(&a2));
    }

    #[test]
    fn compare_phase1_both_agree() {
        let anchor = EvidenceAnchor::CausalEvent { id: "evt_abc".to_string() };
        let bf = make_finding(FindingType::CredentialLeak, anchor.clone());
        let cf = make_finding(FindingType::CredentialLeak, anchor.clone());

        let baseline = RawSweepFindings {
            credential: vec![bf.clone()],
            ..Default::default()
        };
        let current = RawSweepFindings {
            credential: vec![cf.clone()],
            ..Default::default()
        };

        let (agreed, disagreements) = compare_phase1(
            &baseline, &current, "2026-05-07T00:00:00Z", "baseline-rev", "current-rev",
        )
        .unwrap();

        assert!(agreed.contains(&cf.finding_id), "current finding must be agreed");
        assert!(disagreements.is_empty(), "no disagreements when both agree");
    }

    #[test]
    fn compare_phase1_baseline_only() {
        let anchor = EvidenceAnchor::CausalEvent { id: "evt_xyz".to_string() };
        let bf = make_finding(FindingType::CredentialLeak, anchor);

        let baseline = RawSweepFindings {
            credential: vec![bf],
            ..Default::default()
        };
        let current = RawSweepFindings::default();

        let (agreed, disagreements) = compare_phase1(
            &baseline, &current, "2026-05-07T00:00:00Z", "baseline-rev", "current-rev",
        )
        .unwrap();

        assert!(agreed.is_empty());
        assert_eq!(disagreements.len(), 1);
        assert_eq!(disagreements[0].direction, DisagreementDirection::BaselineOnly);
        assert!(disagreements[0].baseline_finding_id.is_some());
        assert!(disagreements[0].current_finding_id.is_none());
    }

    #[test]
    fn compare_phase1_current_only() {
        let anchor = EvidenceAnchor::CausalEvent { id: "evt_new".to_string() };
        let cf = make_finding(FindingType::SandboxEscapeAttempt, anchor);

        let baseline = RawSweepFindings::default();
        let current = RawSweepFindings {
            sandbox_escape: vec![cf.clone()],
            ..Default::default()
        };

        let (agreed, disagreements) = compare_phase1(
            &baseline, &current, "2026-05-07T00:00:00Z", "baseline-rev", "current-rev",
        )
        .unwrap();

        assert!(agreed.is_empty());
        assert_eq!(disagreements.len(), 1);
        assert_eq!(disagreements[0].direction, DisagreementDirection::CurrentOnly);
        assert!(disagreements[0].current_finding_id.is_some());
        assert!(disagreements[0].baseline_finding_id.is_none());
    }

    #[test]
    fn annotate_baseline_agreed_sets_flag() {
        let anchor = EvidenceAnchor::CausalEvent { id: "evt_abc".to_string() };
        let f = make_finding(FindingType::CredentialLeak, anchor);
        let id = f.finding_id.clone();

        let mut raw = RawSweepFindings {
            credential: vec![f],
            ..Default::default()
        };
        let agreed: HashSet<String> = [id].into();
        annotate_baseline_agreed(&mut raw, &agreed);

        assert!(raw.credential[0].baseline_agreed, "baseline_agreed must be true after annotation");
    }

    #[test]
    fn phase2_findings_excluded_from_disagreement() {
        // Phase-2 (LLM-judgment) findings in current must not generate current_only disagreements.
        use autonoetic_types::security::Reproducibility;
        let anchor = EvidenceAnchor::SkillMdDigest { value: "abc".to_string() };
        let llm_finding = SecurityFinding::new(
            FindingType::PromptInjectionSurface,
            FindingSeverity::Warning,
            0.6,
            Reproducibility::LlmJudgment,
            "review",
            "current-rev",
        )
        .with_anchors(vec![anchor]);

        let baseline = RawSweepFindings::default();
        let current = RawSweepFindings {
            // Phase-2 findings go in prompt_injection, not compared by compare_phase1
            prompt_injection: vec![llm_finding],
            ..Default::default()
        };

        let (agreed, disagreements) = compare_phase1(
            &baseline, &current, "2026-05-07T00:00:00Z", "baseline-rev", "current-rev",
        )
        .unwrap();

        assert!(agreed.is_empty());
        assert!(
            disagreements.is_empty(),
            "Phase-2 LLM-judgment findings must not generate disagreements"
        );
    }
}
