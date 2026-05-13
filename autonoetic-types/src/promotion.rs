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
#[serde(rename_all = "lowercase")]
pub enum PromotionRole {
    Evaluator,
    Auditor,
    StaticEvaluator,
    UnitTestRunner,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionQueryArgs {
    /// Artifact ID to query promotion status for.
    pub artifact_id: String,
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
