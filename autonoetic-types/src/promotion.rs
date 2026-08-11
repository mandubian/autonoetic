//! Content Promotion Registry types.
//!
//! Tracks promotion status (evaluator/auditor validation) per artifact.

use serde::{Deserialize, Serialize};

/// A finding from evaluator or auditor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: FindingSeverity,
    pub description: String,
    pub evidence: Option<String>,
}

/// Severity level of a finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

/// Role that recorded the promotion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PromotionRole {
    #[serde(rename = "evaluator")]
    Evaluator,
    #[serde(rename = "auditor")]
    Auditor,
    #[serde(rename = "static_evaluator")]
    StaticEvaluator,
    #[serde(rename = "unit_test_runner")]
    UnitTestRunner,
    #[serde(rename = "sealed_evaluator")]
    SealedEvaluator,
}

impl PromotionRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            PromotionRole::Evaluator => "evaluator",
            PromotionRole::Auditor => "auditor",
            PromotionRole::StaticEvaluator => "static_evaluator",
            PromotionRole::UnitTestRunner => "unit_test_runner",
            PromotionRole::SealedEvaluator => "sealed_evaluator",
        }
    }

    /// Derive the promotion role from a target agent id (e.g.
    /// `"unit_test_runner.default"` → `UnitTestRunner`). Used to fill in
    /// `promotion_role` mechanically when the spawning agent omitted it —
    /// the validation gate otherwise defaults to `"evaluator"` and reports
    /// a phantom `pass=false` for a verdict the child recorded under its
    /// own role.
    pub fn for_agent_id(agent_id: &str) -> Option<PromotionRole> {
        let base = agent_id.split('.').next().unwrap_or(agent_id);
        match base {
            "evaluator" => Some(PromotionRole::Evaluator),
            "auditor" => Some(PromotionRole::Auditor),
            "static_evaluator" => Some(PromotionRole::StaticEvaluator),
            "unit_test_runner" => Some(PromotionRole::UnitTestRunner),
            "sealed_evaluator" => Some(PromotionRole::SealedEvaluator),
            _ => None,
        }
    }
}

/// Promotion record linking validation results to an artifact.
///
/// Role-specific fields are hardcoded for security — the gateway knows exactly
/// which slots exist and which agents may write to them. Adding a new role
/// requires adding a field here plus a match arm in `get_role_result()`, but
/// no gateway code changes (callers use `get_role_result` instead of matching
/// on field names).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub artifact_id: String,
    #[serde(default)]
    pub artifact_digest: Option<String>,
    #[serde(default)]
    pub content_digest: Option<String>,
    #[serde(default)]
    pub evaluator_id: Option<String>,
    #[serde(default)]
    pub evaluator_pass: bool,
    #[serde(default)]
    pub evaluator_findings: Vec<Finding>,
    #[serde(default)]
    pub evaluator_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_execution_trace_id: Option<String>,
    #[serde(default)]
    pub auditor_id: Option<String>,
    #[serde(default)]
    pub auditor_pass: bool,
    #[serde(default)]
    pub auditor_findings: Vec<Finding>,
    #[serde(default)]
    pub auditor_timestamp: Option<String>,
    #[serde(default)]
    pub static_evaluator_id: Option<String>,
    #[serde(default)]
    pub static_evaluator_pass: bool,
    #[serde(default)]
    pub static_evaluator_findings: Vec<Finding>,
    #[serde(default)]
    pub static_evaluator_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_evaluator_execution_trace_id: Option<String>,
    #[serde(default)]
    pub unit_test_runner_id: Option<String>,
    #[serde(default)]
    pub unit_test_runner_pass: bool,
    #[serde(default)]
    pub unit_test_runner_findings: Vec<Finding>,
    #[serde(default)]
    pub unit_test_runner_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_test_runner_execution_trace_id: Option<String>,
    #[serde(default)]
    pub sealed_evaluator_id: Option<String>,
    #[serde(default)]
    pub sealed_evaluator_pass: bool,
    #[serde(default)]
    pub sealed_evaluator_findings: Vec<Finding>,
    #[serde(default)]
    pub sealed_evaluator_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_evaluator_execution_trace_id: Option<String>,
    pub promotion_gate_version: String,
    /// The resolved dependency closure (name==version) **blessed** at promotion:
    /// the versions the validated, approved run actually used, frozen here so the
    /// pin is earned by validation rather than demanded up front. Empty until
    /// blessed, or for agents with no dependency layers.
    /// See `docs/design/packager-dependency-determinism.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blessed_packages: Vec<crate::layer::ResolvedPackage>,

    // --- Federation carry-forward digests (Stage 1, see
    //     docs/federation-carry-forward.md — the design spec landing with
    //     #1067) ---
    //
    // The three per-input digests of the artifact this verdict was recorded
    // against. `None` for records predating this feature (verdict unverifiable
    // → must re-run) and for non-agent-bundle artifacts. Copied from the
    // artifact at `promotion.record` time so the verdict binds to the exact
    // bytes the gate reviewed.
    //
    // Stage 3 will add `carried_from` provenance when a verdict is carried
    // forward from a prior artifact rather than freshly run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prose_digest: Option<String>,
}

impl PromotionRecord {
    /// Look up a role's verdict by role name string.
    ///
    /// Centralizes the role→field mapping so callers (`validate_promotion_record`,
    /// `has_passed`) don't duplicate match arms. Returns `(pass, &findings)`.
    pub fn get_role_result(&self, role: &str) -> Option<(bool, &[Finding])> {
        match role {
            "evaluator" => Some((self.evaluator_pass, &self.evaluator_findings)),
            "auditor" => Some((self.auditor_pass, &self.auditor_findings)),
            "static_evaluator" => Some((self.static_evaluator_pass, &self.static_evaluator_findings)),
            "unit_test_runner" => Some((self.unit_test_runner_pass, &self.unit_test_runner_findings)),
            "sealed_evaluator" => Some((self.sealed_evaluator_pass, &self.sealed_evaluator_findings)),
            _ => None,
        }
    }

    /// Whether any agent has recorded a verdict in the given role slot.
    /// Distinguishes "no verdict yet" from an explicit `pass=false`.
    pub fn has_role_verdict(&self, role: &str) -> bool {
        match role {
            "evaluator" => self.evaluator_id.is_some(),
            "auditor" => self.auditor_id.is_some(),
            "static_evaluator" => self.static_evaluator_id.is_some(),
            "unit_test_runner" => self.unit_test_runner_id.is_some(),
            "sealed_evaluator" => self.sealed_evaluator_id.is_some(),
            _ => false,
        }
    }

    /// Role-slot names that currently hold a recorded verdict.
    pub fn roles_with_verdicts(&self) -> Vec<&'static str> {
        let mut roles = Vec::new();
        if self.evaluator_id.is_some() {
            roles.push("evaluator");
        }
        if self.auditor_id.is_some() {
            roles.push("auditor");
        }
        if self.static_evaluator_id.is_some() {
            roles.push("static_evaluator");
        }
        if self.unit_test_runner_id.is_some() {
            roles.push("unit_test_runner");
        }
        if self.sealed_evaluator_id.is_some() {
            roles.push("sealed_evaluator");
        }
        roles
    }
}

/// Arguments for the `promotion.record` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionRecordArgs {
    /// Artifact ID being promoted (e.g., 'art_a1b2c3d4'). Required if
    /// `artifact_ref` is not set; when both are set they must resolve to
    /// the same canonical ID.
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// Short artifact ref (e.g., 'ar.386f5b222421'). Alternative to
    /// `artifact_id`; resolved server-side to the canonical `art_*` ID.
    /// Prefer this when you only have the ref, e.g. from a spawn task.
    #[serde(default)]
    pub artifact_ref: Option<String>,
    /// SHA256 digest of the artifact (optional, for integrity verification).
    #[serde(default)]
    pub artifact_digest: Option<String>,
    /// Reserved gateway-owned binding for canonical revision content.
    /// External callers must omit this field; `promotion.record` rejects it.
    #[serde(default)]
    pub content_digest: Option<String>,
    /// Role recording this promotion (evaluator or auditor).
    pub role: PromotionRole,
    /// Whether this role's validation passed. Required for `auditor`; ignored for
    /// execution roles (derived from `execution_trace_id`).
    #[serde(default)]
    pub pass: Option<bool>,
    /// Execution trace id for roles that run code (`unit_test_runner`,
    /// `static_evaluator`, `sealed_evaluator`, legacy `evaluator`). Required for
    /// those roles; `pass` is derived from `exit_code`.
    #[serde(default)]
    pub execution_trace_id: Option<String>,
    /// Findings from this validation.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Human-readable summary of the validation.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Response from the `promotion.record` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionRecordResponse {
    pub ok: bool,
    pub promotion_record: PromotionRecord,
}

/// Arguments for the `promotion.query` tool.
///
/// At least one of `artifact_id` or `artifact_ref` must be supplied. Both
/// are optional at the serde level so callers can use either form; the
/// runtime tool checks that at least one is present and rejects the
/// "neither" case with a clear error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionQueryArgs {
    /// Canonical artifact ID (e.g., `art_a1b2c3d4`). Alternative: `artifact_ref`.
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// Short artifact ref (e.g., `ar.386f5b222421`). Alternative: `artifact_id`.
    #[serde(default)]
    pub artifact_ref: Option<String>,
}

/// Response from the `promotion.query` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionQueryResponse {
    pub artifact_id: String,
    #[serde(default)]
    pub content_digest: Option<String>,
    pub evaluator_pass: Option<bool>,
    pub auditor_pass: Option<bool>,
    pub evaluator_id: Option<String>,
    pub auditor_id: Option<String>,
    pub evaluator_findings: Vec<Finding>,
    pub auditor_findings: Vec<Finding>,
    pub evaluator_timestamp: Option<String>,
    pub auditor_timestamp: Option<String>,
    pub static_evaluator_pass: Option<bool>,
    pub static_evaluator_id: Option<String>,
    pub static_evaluator_findings: Vec<Finding>,
    pub static_evaluator_timestamp: Option<String>,
    pub unit_test_runner_pass: Option<bool>,
    pub unit_test_runner_id: Option<String>,
    pub unit_test_runner_findings: Vec<Finding>,
    pub unit_test_runner_timestamp: Option<String>,
    pub sealed_evaluator_pass: Option<bool>,
    pub sealed_evaluator_id: Option<String>,
    pub sealed_evaluator_findings: Vec<Finding>,
    pub sealed_evaluator_timestamp: Option<String>,
    pub promotion_gate_version: String,
}

#[cfg(test)]
mod promotion_query_args_tests {
    use super::PromotionQueryArgs;

    // Regression: prior version had `artifact_id: String` (required). Agents
    // passing only `artifact_ref` got serde "missing field" errors, which
    // contradicted the tool's input_schema (both advertised as alternatives).
    // See session-3b4485d4 — five LoopGuard cycles burned on this mismatch.

    #[test]
    fn parses_artifact_ref_only() {
        let args: PromotionQueryArgs =
            serde_json::from_str(r#"{"artifact_ref": "ar.dd5058d99426"}"#).expect("should parse");
        assert!(args.artifact_id.is_none());
        assert_eq!(args.artifact_ref.as_deref(), Some("ar.dd5058d99426"));
    }

    #[test]
    fn parses_artifact_id_only() {
        let args: PromotionQueryArgs =
            serde_json::from_str(r#"{"artifact_id": "art_dd5058d9"}"#).expect("should parse");
        assert_eq!(args.artifact_id.as_deref(), Some("art_dd5058d9"));
        assert!(args.artifact_ref.is_none());
    }

    #[test]
    fn parses_both_fields() {
        let args: PromotionQueryArgs = serde_json::from_str(
            r#"{"artifact_id": "art_dd5058d9", "artifact_ref": "ar.dd5058d99426"}"#,
        )
        .expect("should parse");
        assert_eq!(args.artifact_id.as_deref(), Some("art_dd5058d9"));
        assert_eq!(args.artifact_ref.as_deref(), Some("ar.dd5058d99426"));
    }

    #[test]
    fn parses_empty_object() {
        let args: PromotionQueryArgs =
            serde_json::from_str("{}").expect("should parse empty object");
        assert!(args.artifact_id.is_none());
        assert!(args.artifact_ref.is_none());
    }
}

#[cfg(test)]
mod promotion_record_tests {
    use super::{PromotionRecord, PromotionRole};

    #[test]
    fn loads_old_json_without_new_fields() {
        let old_json = r#"{
            "artifact_id": "art_abc",
            "evaluator_pass": true,
            "evaluator_id": "evaluator.default",
            "evaluator_findings": [],
            "evaluator_timestamp": "2026-01-01T00:00:00Z",
            "auditor_pass": false,
            "promotion_gate_version": "1.0"
        }"#;
        let record: PromotionRecord =
            serde_json::from_str(old_json).expect("old JSON should still load");
        assert!(record.evaluator_pass);
        assert!(!record.auditor_pass);
    }

    #[test]
    fn get_role_result_returns_correct_slots() {
        let record: PromotionRecord = serde_json::from_str(r#"{
            "artifact_id": "art_test",
            "evaluator_pass": true,
            "auditor_pass": false,
            "static_evaluator_pass": true,
            "unit_test_runner_pass": true,
            "sealed_evaluator_pass": false,
            "promotion_gate_version": "2.0"
        }"#).expect("should load");

        assert_eq!(record.get_role_result("evaluator").map(|(p, f)| (p, f.is_empty())), Some((true, true)));
        assert_eq!(record.get_role_result("auditor").map(|(p, f)| (p, f.is_empty())), Some((false, true)));
        assert_eq!(record.get_role_result("static_evaluator").map(|(p, f)| (p, f.is_empty())), Some((true, true)));
        assert_eq!(record.get_role_result("unit_test_runner").map(|(p, f)| (p, f.is_empty())), Some((true, true)));
        assert_eq!(record.get_role_result("sealed_evaluator").map(|(p, f)| (p, f.is_empty())), Some((false, true)));
        assert!(record.get_role_result("unknown_role").is_none());
    }

    #[test]
    fn for_agent_id_maps_gate_agents() {
        assert_eq!(
            PromotionRole::for_agent_id("unit_test_runner.default").map(|r| r.as_str()),
            Some("unit_test_runner")
        );
        assert_eq!(
            PromotionRole::for_agent_id("auditor.default").map(|r| r.as_str()),
            Some("auditor")
        );
        assert_eq!(
            PromotionRole::for_agent_id("static_evaluator.default").map(|r| r.as_str()),
            Some("static_evaluator")
        );
        assert_eq!(
            PromotionRole::for_agent_id("sealed_evaluator.default").map(|r| r.as_str()),
            Some("sealed_evaluator")
        );
        assert_eq!(
            PromotionRole::for_agent_id("evaluator.default").map(|r| r.as_str()),
            Some("evaluator")
        );
        assert!(PromotionRole::for_agent_id("coder.default").is_none());
        assert!(PromotionRole::for_agent_id("planner.default").is_none());
        // Bare ids without a `.default` suffix resolve the same way.
        assert_eq!(
            PromotionRole::for_agent_id("unit_test_runner").map(|r| r.as_str()),
            Some("unit_test_runner")
        );
    }

    #[test]
    fn has_role_verdict_and_roles_with_verdicts_track_recorded_slots() {
        let record: PromotionRecord = serde_json::from_str(r#"{
            "artifact_id": "art_test",
            "evaluator_id": "evaluator.default",
            "evaluator_pass": true,
            "unit_test_runner_id": "unit_test_runner.default",
            "unit_test_runner_pass": true,
            "promotion_gate_version": "2.0"
        }"#).expect("should load");

        assert!(record.has_role_verdict("evaluator"));
        assert!(record.has_role_verdict("unit_test_runner"));
        assert!(!record.has_role_verdict("auditor"));
        assert!(!record.has_role_verdict("static_evaluator"));
        assert!(!record.has_role_verdict("unknown"));
        assert_eq!(
            record.roles_with_verdicts(),
            vec!["evaluator", "unit_test_runner"]
        );
    }
}
