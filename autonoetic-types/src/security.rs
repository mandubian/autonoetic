//! Security sentinel types: `SecurityFinding` and related enums.

use serde::{Deserialize, Serialize};

/// Severity of a security finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Critical,
    Warning,
    Info,
}

impl std::fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindingSeverity::Critical => write!(f, "critical"),
            FindingSeverity::Warning => write!(f, "warning"),
            FindingSeverity::Info => write!(f, "info"),
        }
    }
}

/// Category of a security finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingType {
    CredentialLeak,
    CapabilityAccretion,
    SandboxEscapeAttempt,
    ApprovalBypass,
    PromptInjectionSurface,
    SupplyChainScopeViolation,
    BehavioralAnomaly,
    CuratorBias,
}

impl std::fmt::Display for FindingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{:?}", self));
        write!(f, "{}", s)
    }
}

/// How reproducible the finding is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reproducibility {
    /// A regex, SQL query, or structural comparison — always produces the same answer.
    Deterministic,
    /// An LLM reasoning pass — may vary between runs.
    LlmJudgment,
    /// A statistical anomaly detection — depends on the baseline sample.
    Statistical,
}

/// A reference to a specific piece of evidence backing a finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceAnchor {
    CausalEvent { id: String },
    SkillMdDigest { value: String },
    LayerDigest { value: String },
    ArtifactId { id: String },
    RevisionId { id: String },
}

/// Which entities are implicated in a finding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AffectedEntities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_digest: Option<String>,
}

/// A structured security finding produced by the security sentinel.
///
/// Findings are append-only: once recorded they are never modified. Triage
/// state is a separate column (`triage_state`, `triage_reason`) so the
/// original finding body is preserved verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityFinding {
    pub finding_id: String,
    pub severity: FindingSeverity,
    /// 0.0–1.0 confidence score.
    pub confidence: f64,
    pub finding_type: FindingType,
    pub affected: AffectedEntities,
    pub evidence_anchors: Vec<EvidenceAnchor>,
    pub reproducibility: Reproducibility,
    pub proposed_remediation: String,
    /// Revision ID of the sentinel that produced this finding.
    pub sentinel_revision_id: String,
    /// Whether the frozen baseline sentinel also flagged the same anchor.
    pub baseline_agreed: bool,
    /// Whether a second-model ensemble pass also agreed (None = not yet run).
    pub ensemble_agreed: Option<bool>,
}

impl SecurityFinding {
    pub fn new(
        finding_type: FindingType,
        severity: FindingSeverity,
        confidence: f64,
        reproducibility: Reproducibility,
        proposed_remediation: impl Into<String>,
        sentinel_revision_id: impl Into<String>,
    ) -> Self {
        Self {
            finding_id: format!("sec_{}", uuid::Uuid::new_v4()),
            severity,
            confidence,
            finding_type,
            affected: AffectedEntities::default(),
            evidence_anchors: Vec::new(),
            reproducibility,
            proposed_remediation: proposed_remediation.into(),
            sentinel_revision_id: sentinel_revision_id.into(),
            baseline_agreed: false,
            ensemble_agreed: None,
        }
    }

    pub fn with_affected(mut self, affected: AffectedEntities) -> Self {
        self.affected = affected;
        self
    }

    pub fn with_anchors(mut self, anchors: Vec<EvidenceAnchor>) -> Self {
        self.evidence_anchors = anchors;
        self
    }

    pub fn with_baseline_agreed(mut self, agreed: bool) -> Self {
        self.baseline_agreed = agreed;
        self
    }
}

/// Triage state for an operator reviewing a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageState {
    Pending,
    TruePositive,
    FalsePositive,
    Benign,
    Deferred,
}

impl std::fmt::Display for TriageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TriageState::Pending => "pending",
            TriageState::TruePositive => "true_positive",
            TriageState::FalsePositive => "false_positive",
            TriageState::Benign => "benign",
            TriageState::Deferred => "deferred",
        };
        write!(f, "{}", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_finding_round_trips_json() {
        let finding = SecurityFinding::new(
            FindingType::CredentialLeak,
            FindingSeverity::Critical,
            1.0,
            Reproducibility::Deterministic,
            "rotate the credential",
            "sentinel-rev-abc123",
        )
        .with_affected(AffectedEntities {
            agent_alias: Some("coder.default".to_string()),
            session_id: Some("sess_abc".to_string()),
            ..Default::default()
        })
        .with_anchors(vec![
            EvidenceAnchor::CausalEvent { id: "evt_001".to_string() },
        ]);

        let json = serde_json::to_string(&finding).expect("serialize");
        let back: SecurityFinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.finding_id, finding.finding_id);
        assert_eq!(back.severity, FindingSeverity::Critical);
        assert_eq!(back.finding_type, FindingType::CredentialLeak);
        assert_eq!(back.reproducibility, Reproducibility::Deterministic);
        assert_eq!(back.evidence_anchors.len(), 1);
    }

    #[test]
    fn triage_state_display() {
        assert_eq!(TriageState::Pending.to_string(), "pending");
        assert_eq!(TriageState::TruePositive.to_string(), "true_positive");
        assert_eq!(TriageState::FalsePositive.to_string(), "false_positive");
    }
}
