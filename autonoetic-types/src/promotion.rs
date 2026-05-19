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
}

/// Promotion record linking validation results to an artifact.
///
/// TODO: When a 6th role is added, refactor to `HashMap<PromotionRole, RoleVerdict>`
/// per plan §3.3. Custom `Deserialize` reads both old flat format and new map.
/// On first write after upgrade, normalise to the new format. No data migration
/// script needed; read-time migration only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionRecord {
    /// Artifact ID this promotion applies to (e.g., "art_a1b2c3d4").
    pub artifact_id: String,
    /// SHA256 digest of the artifact at review time (for integrity verification).
    #[serde(default)]
    pub artifact_digest: Option<String>,
    /// Canonical revision content digest this promotion evidence is bound to.
    #[serde(default)]
    pub content_digest: Option<String>,
    /// Agent who validated (evaluator.default).
    #[serde(default)]
    pub evaluator_id: Option<String>,
    /// Whether evaluator passed.
    #[serde(default)]
    pub evaluator_pass: bool,
    /// Findings from evaluator.
    #[serde(default)]
    pub evaluator_findings: Vec<Finding>,
    /// Timestamp of evaluator validation (ISO 8601).
    #[serde(default)]
    pub evaluator_timestamp: Option<String>,
    /// Agent who audited (auditor.default).
    #[serde(default)]
    pub auditor_id: Option<String>,
    /// Whether auditor passed.
    #[serde(default)]
    pub auditor_pass: bool,
    /// Findings from auditor.
    #[serde(default)]
    pub auditor_findings: Vec<Finding>,
    /// Timestamp of auditor validation (ISO 8601).
    #[serde(default)]
    pub auditor_timestamp: Option<String>,
    /// Static evaluator agent (static_evaluator.default).
    #[serde(default)]
    pub static_evaluator_id: Option<String>,
    /// Whether static evaluator passed.
    #[serde(default)]
    pub static_evaluator_pass: bool,
    /// Findings from static evaluator.
    #[serde(default)]
    pub static_evaluator_findings: Vec<Finding>,
    /// Timestamp of static evaluator validation (ISO 8601).
    #[serde(default)]
    pub static_evaluator_timestamp: Option<String>,
    /// Unit test runner agent (unit_test_runner.default).
    #[serde(default)]
    pub unit_test_runner_id: Option<String>,
    /// Whether unit test runner passed.
    #[serde(default)]
    pub unit_test_runner_pass: bool,
    /// Findings from unit test runner.
    #[serde(default)]
    pub unit_test_runner_findings: Vec<Finding>,
    /// Timestamp of unit test runner validation (ISO 8601).
    #[serde(default)]
    pub unit_test_runner_timestamp: Option<String>,
    /// Sealed evaluator agent (sealed_evaluator.default).
    #[serde(default)]
    pub sealed_evaluator_id: Option<String>,
    /// Whether sealed evaluator passed.
    #[serde(default)]
    pub sealed_evaluator_pass: bool,
    /// Findings from sealed evaluator.
    #[serde(default)]
    pub sealed_evaluator_findings: Vec<Finding>,
    /// Timestamp of sealed evaluator validation (ISO 8601).
    #[serde(default)]
    pub sealed_evaluator_timestamp: Option<String>,
    /// Version of promotion gate schema.
    pub promotion_gate_version: String,
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
    /// Whether this role's validation passed.
    pub pass: bool,
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
        // Serde accepts both fields missing; the runtime tool rejects "neither"
        // with a clear error message rather than letting serde say
        // "missing field `artifact_id`".
        let args: PromotionQueryArgs =
            serde_json::from_str("{}").expect("should parse empty object");
        assert!(args.artifact_id.is_none());
        assert!(args.artifact_ref.is_none());
    }
}
