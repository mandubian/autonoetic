//! Background scheduling and reevaluation types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundMode {
    Deterministic,
    Reasoning,
}

impl Default for BackgroundMode {
    fn default() -> Self {
        Self::Deterministic
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WakePredicates {
    #[serde(default = "default_true")]
    pub timer: bool,
    #[serde(default)]
    pub approval_resolved: bool,
}

impl Default for WakePredicates {
    fn default() -> Self {
        Self {
            timer: true,
            approval_resolved: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BackgroundPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub interval_secs: u64,
    #[serde(default)]
    pub mode: BackgroundMode,
    #[serde(default)]
    pub wake_predicates: WakePredicates,
    #[serde(default = "default_true")]
    pub validate_on_install: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledActionDependencies {
    pub runtime: String,
    pub packages: Vec<String>,
}

/// Actions that can be stored in reevaluation state and executed by the background scheduler,
/// or used as the *subject* of an approval request (ApprovalRequest/ApprovalDecision).
///
/// **Schedulable vs approval-only:**
/// - `WriteFile` and `SandboxExec` are real runnable actions: the scheduler can execute them
///   (after approval when `requires_approval` is true). They may also appear in
///   `pending_scheduled_action` and in approval requests.
/// - `AgentInstall` is **not** something the scheduler runs. We cannot "schedule" the
///   installation of an agent. It exists only as the subject of an approval request: when
///   `agent.install` requires human approval, we create an approval with action=AgentInstall;
///   after the operator approves, the *caller* retries `agent.install` with
///   `install_approval_ref` and the install runs synchronously. The scheduler never executes
///   an AgentInstall; it is only a label for "this approval was for an install."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduledAction {
    WriteFile {
        path: String,
        content: String,
        #[serde(default)]
        requires_approval: bool,
        #[serde(default)]
        evidence_ref: Option<String>,
    },
    SandboxExec {
        command: String,
        #[serde(default)]
        dependencies: Option<ScheduledActionDependencies>,
        #[serde(default)]
        requires_approval: bool,
        #[serde(default)]
        evidence_ref: Option<String>,
    },
    /// Approval subject only: "this approval request is for an agent install." Not executed by the scheduler; install is performed by the caller retrying `agent.install` with `install_approval_ref`.
    AgentInstall {
        agent_id: String,
        summary: String,
        requested_by_agent_id: String,
        install_fingerprint: String,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Approval subject only: "this approval request is for a credential setup UserPrompt step."
    /// Not executed by the scheduler; the operator provides secrets through the approval channel,
    /// then the caller retries `credential.setup` with `approval_ref`.
    CredentialPrompt {
        service: String,
        credential_id: String,
        message: String,
        secret_fields: Vec<super::agent::SecretFieldSpec>,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Approval subject only: continue a session after max-turn circuit breaker trips.
    /// Not executed by the scheduler; once approved, the next resume attempt proceeds.
    SessionContinue {
        session_id: String,
        root_session_id: String,
        requested_by_agent_id: String,
        turn_counter: u64,
        max_turns: u32,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
}

impl ScheduledAction {
    /// True if this action is something the scheduler can execute (WriteFile, SandboxExec).
    /// False for AgentInstall and CredentialPrompt, which are only approval subjects.
    pub fn is_executable_by_scheduler(&self) -> bool {
        !matches!(
            self,
            Self::AgentInstall { .. } | Self::CredentialPrompt { .. } | Self::SessionContinue { .. }
        )
    }

    pub fn requires_approval(&self) -> bool {
        match self {
            Self::WriteFile {
                requires_approval, ..
            }
            | Self::SandboxExec {
                requires_approval, ..
            } => *requires_approval,
            Self::AgentInstall { .. }
            | Self::CredentialPrompt { .. }
            | Self::SessionContinue { .. } => true,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::WriteFile { .. } => "write_file",
            Self::SandboxExec { .. } => "sandbox_exec",
            Self::AgentInstall { .. } => "agent_install",
            Self::CredentialPrompt { .. } => "credential_prompt",
            Self::SessionContinue { .. } => "session_continue",
        }
    }

    pub fn evidence_ref(&self) -> Option<String> {
        match self {
            Self::WriteFile { evidence_ref, .. } => evidence_ref.clone(),
            Self::SandboxExec { evidence_ref, .. } => evidence_ref.clone(),
            Self::AgentInstall { .. }
            | Self::CredentialPrompt { .. }
            | Self::SessionContinue { .. } => None,
        }
    }

    pub fn with_evidence_ref(mut self, evidence_ref: Option<String>) -> Self {
        match &mut self {
            Self::WriteFile {
                evidence_ref: r, ..
            } => *r = evidence_ref,
            Self::SandboxExec {
                evidence_ref: r, ..
            } => *r = evidence_ref,
            Self::AgentInstall { .. }
            | Self::CredentialPrompt { .. }
            | Self::SessionContinue { .. } => {}
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ReevaluationState {
    #[serde(default)]
    pub retry_not_before: Option<String>,
    #[serde(default)]
    pub stale_goal_at: Option<String>,
    #[serde(default)]
    pub last_outcome: Option<String>,
    #[serde(default)]
    pub pending_scheduled_action: Option<ScheduledAction>,
    #[serde(default)]
    pub open_approval_request_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeReason {
    Timer { due_bucket: String },
    ApprovalResolved { request_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BackgroundState {
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub last_wake_at: Option<String>,
    #[serde(default)]
    pub last_wake_reason: Option<WakeReason>,
    #[serde(default)]
    pub last_result: Option<String>,
    #[serde(default)]
    pub next_due_at: Option<String>,
    #[serde(default)]
    pub active_session_ids: Vec<String>,
    #[serde(default)]
    pub pending_wake_fingerprints: Vec<String>,
    #[serde(default)]
    pub retry_not_before: Option<String>,
    #[serde(default)]
    pub approval_blocked: bool,
    #[serde(default)]
    pub pending_approval_request_ids: Vec<String>,
    #[serde(default)]
    pub processed_approval_request_ids: Vec<String>,
}

/// A request for human approval. The `action` describes what is being approved: either a
/// schedulable action (WriteFile, SandboxExec) that the scheduler will run after approval, or
/// an approval-only subject (AgentInstall) where the actual install is done by the caller
/// retrying with install_approval_ref.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub action: ScheduledAction,
    pub created_at: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub evidence_ref: Option<String>,
    #[serde(default)]
    pub root_session_id: Option<String>,
    /// Workflow this approval belongs to (for task-level unblocking on resolution).
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// Task this approval blocks (unblocked on approval resolution).
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub status: Option<ApprovalStatus>,
    #[serde(default)]
    pub decided_at: Option<String>,
    #[serde(default)]
    pub decided_by: Option<String>,
    /// Required approval level for this request (operator, admin, agent:xyz).
    /// Defaults to Operator. Set by the gateway based on config escalation rules.
    #[serde(default)]
    pub approval_level: ApprovalLevel,
}

impl ApprovalRequest {
    pub fn into_decision(self) -> anyhow::Result<ApprovalDecision> {
        let status = self
            .status
            .ok_or_else(|| anyhow::anyhow!("Approval status is missing"))?;
        let decided_at = self
            .decided_at
            .ok_or_else(|| anyhow::anyhow!("Decided at is missing"))?;
        let decided_by = self
            .decided_by
            .ok_or_else(|| anyhow::anyhow!("Decided by is missing"))?;

        Ok(ApprovalDecision {
            request_id: self.request_id,
            agent_id: self.agent_id,
            session_id: self.session_id,
            action: self.action,
            status,
            decided_at,
            decided_by,
            reason: self.reason,
            root_session_id: self.root_session_id,
            workflow_id: self.workflow_id,
            task_id: self.task_id,
            approval_level: self.approval_level,
        })
    }
}

/// Approval level for escalation control.
/// Determines who is authorized to resolve an approval request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLevel {
    /// Standard operator approval (default).
    #[default]
    Operator,
    /// Requires admin-level authorization.
    Admin,
    /// Only a specific agent can approve. e.g. Agent("auditor.default")
    Agent(String),
}

impl ApprovalLevel {
    /// Parse from config string. Returns Operator for unrecognized values.
    pub fn from_config(s: &str) -> Self {
        match s {
            "admin" => Self::Admin,
            s if s.starts_with("agent:") => Self::Agent(s[6..].to_string()),
            _ => Self::Operator,
        }
    }

    /// Serialize to config string.
    pub fn to_config(&self) -> String {
        match self {
            Self::Operator => "operator".to_string(),
            Self::Admin => "admin".to_string(),
            Self::Agent(id) => format!("agent:{}", id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Approved,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub request_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub action: ScheduledAction,
    pub status: ApprovalStatus,
    pub decided_at: String,
    pub decided_by: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub root_session_id: Option<String>,
    /// Workflow this approval belongs to (copied from ApprovalRequest).
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// Task this approval blocks (copied from ApprovalRequest).
    #[serde(default)]
    pub task_id: Option<String>,
    /// Required approval level for this request (copied from ApprovalRequest).
    #[serde(default)]
    pub approval_level: ApprovalLevel,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// User Interaction
// ---------------------------------------------------------------------------

/// Why the agent asked the user something.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserInteractionKind {
    /// "What did you mean by X?"
    Clarification,
    /// "I can do A or B — which do you prefer?"
    Decision,
    /// "Here's my proposal — approve or suggest changes?"
    Proposal,
    /// "Do you want to proceed with this?"
    Confirmation,
}

impl std::fmt::Display for UserInteractionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clarification => write!(f, "clarification"),
            Self::Decision => write!(f, "decision"),
            Self::Proposal => write!(f, "proposal"),
            Self::Confirmation => write!(f, "confirmation"),
        }
    }
}

impl UserInteractionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Clarification => "clarification",
            Self::Decision => "decision",
            Self::Proposal => "proposal",
            Self::Confirmation => "confirmation",
        }
    }
}

/// Status of a user interaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UserInteractionStatus {
    Pending,
    Answered,
    Cancelled,
    Expired,
}

/// A single option the agent presents to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInteractionOption {
    pub id: String,
    pub label: String,
    pub value: String,
}

/// An interaction created by `user.ask`.
///
/// Stored in `user_interactions` table. When a user interaction is created,
/// the agent's turn is suspended and a checkpoint is saved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInteraction {
    pub interaction_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub agent_id: String,
    pub turn_id: String,
    pub kind: UserInteractionKind,
    pub question: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub options: Vec<UserInteractionOption>,
    #[serde(default = "default_true")]
    pub allow_freeform: bool,
    pub status: UserInteractionStatus,
    #[serde(default)]
    pub answer_option_id: Option<String>,
    #[serde(default)]
    pub answer_text: Option<String>,
    #[serde(default)]
    pub answered_by: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub answered_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub checkpoint_turn_id: Option<String>,
}

/// The answer provided by the user (via CLI, chat, or API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInteractionAnswer {
    pub interaction_id: String,
    #[serde(default)]
    pub answer_option_id: Option<String>,
    #[serde(default)]
    pub answer_text: Option<String>,
    pub answered_by: String,
}

/// Payload injected into the resumed conversation when a user answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInteractionResumePayload {
    pub interaction_id: String,
    pub kind: UserInteractionKind,
    pub question: String,
    #[serde(default)]
    pub answer_option_id: Option<String>,
    #[serde(default)]
    pub answer_text: Option<String>,
}
