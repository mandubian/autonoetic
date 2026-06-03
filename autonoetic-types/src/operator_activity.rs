//! Channel-neutral operator activity records for live session visibility.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum OperatorActivitySeverity {
    Info,
    Progress,
    Attention,
    Error,
}

impl OperatorActivitySeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Progress => "progress",
            Self::Attention => "attention",
            Self::Error => "error",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "progress" => Some(Self::Progress),
            "attention" => Some(Self::Attention),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperatorActivityKind {
    ToolCompleted,
    ToolFailed,
    Delegation,
    WorkflowJoin,
    ApprovalRequired,
    PlanProposal,
    HumanGate,
    SessionLifecycle,
    /// Synthetic marker emitted when the per-root activity rate limit is hit,
    /// so suppression of subsequent rows in the window is visible.
    RateLimited,
}

impl OperatorActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolCompleted => "tool_completed",
            Self::ToolFailed => "tool_failed",
            Self::Delegation => "delegation",
            Self::WorkflowJoin => "workflow_join",
            Self::ApprovalRequired => "approval_required",
            Self::PlanProposal => "plan_proposal",
            Self::HumanGate => "human_gate",
            Self::SessionLifecycle => "session_lifecycle",
            Self::RateLimited => "rate_limited",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorActivityRefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workbench_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorActivityRecord {
    pub activity_id: String,
    pub root_session_id: String,
    pub session_id: String,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub occurred_at: String,
    pub kind: OperatorActivityKind,
    pub severity: OperatorActivitySeverity,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causal_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "OperatorActivityRefs::is_empty")]
    pub refs: OperatorActivityRefs,
}

impl OperatorActivityRefs {
    fn is_empty(&self) -> bool {
        self.approval_request_id.is_none()
            && self.plan_id.is_none()
            && self.interaction_id.is_none()
            && self.artifact_id.is_none()
            && self.workbench_id.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorActivityListParams {
    pub root_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_activity_id: Option<String>,
    #[serde(default = "default_operator_activity_limit")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<String>,
}

fn default_operator_activity_limit() -> u32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperatorActivityListResult {
    pub activities: Vec<OperatorActivityRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
