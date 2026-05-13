use crate::promotion::PromotionRole;
use serde::{Deserialize, Serialize};

/// A summary of a single federation role's verdict, included in an
/// `EscalationMessage` so the operator sees all role judgments in one place.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleVerdictSummary {
    pub role: PromotionRole,
    pub agent_id: String,
    pub passed: bool,
    pub findings_summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    pub recorded_at: String,
}

/// Status of a federation escalation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EscalationStatus {
    /// Escalation created, awaiting operator review.
    #[default]
    Pending,
    /// Operator approved the escalation (promotion may proceed).
    Approved,
    /// Operator rejected the escalation (promotion blocked).
    Rejected,
}

impl EscalationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            EscalationStatus::Pending => "pending",
            EscalationStatus::Approved => "approved",
            EscalationStatus::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(EscalationStatus::Pending),
            "approved" => Some(EscalationStatus::Approved),
            "rejected" => Some(EscalationStatus::Rejected),
            _ => None,
        }
    }
}

/// Structured message from the planner to the operator carrying federation
/// jury verdicts.  This is the bridge between "planner spawns federation roles
/// and accumulates verdicts" and "operator reviews a consolidated report and
/// makes a decision."
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationMessage {
    /// Unique identifier (esc_xxxxxxxx).
    pub escalation_id: String,
    /// The artifact under review.
    pub artifact_id: String,
    /// Canonical digest of the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    /// The agent being promoted.
    pub agent_id: String,
    /// The revision being proposed for promotion.
    pub revision_id: String,
    /// One summary per federation role that recorded a verdict.
    pub role_verdicts: Vec<RoleVerdictSummary>,
    /// The planner's own summary / recommendation for the operator.
    pub planner_synthesis: String,
    /// When this escalation was created (ISO 8601).
    pub created_at: String,
    /// When the escalation was resolved (ISO 8601).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    /// Root session this escalation belongs to.
    pub root_session_id: String,
    /// Current status.
    #[serde(default)]
    pub status: EscalationStatus,
    /// Who decided (operator ID or admin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// Why the escalation was resolved this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
}

impl EscalationMessage {
    pub fn new(
        escalation_id: String,
        artifact_id: String,
        agent_id: String,
        revision_id: String,
        role_verdicts: Vec<RoleVerdictSummary>,
        planner_synthesis: String,
        root_session_id: String,
    ) -> Self {
        Self {
            escalation_id,
            artifact_id,
            artifact_digest: None,
            agent_id,
            revision_id,
            role_verdicts,
            planner_synthesis,
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved_at: None,
            root_session_id,
            status: EscalationStatus::Pending,
            decided_by: None,
            decision_reason: None,
        }
    }
}
