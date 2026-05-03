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

/// Describes a single layer whose build-time approval scope exceeds the current session's grants.
/// Used in `ScheduledAction::LayerMount` approval requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LayerMountScopeInfo {
    /// Layer identifier.
    pub layer_id: String,
    /// Content digest of the layer archive.
    pub digest: String,
    /// Human-readable layer name.
    pub name: String,
    /// Mount path inside the sandbox.
    pub mount_path: String,
    /// All hosts that were approved when this layer was captured.
    pub build_time_approved_hosts: Vec<String>,
    /// Hosts in `build_time_approved_hosts` not currently covered by this session's grants.
    pub unapproved_delta: Vec<String>,
    /// Where this layer comes from: "artifact:<artifact_id>" or "runtime.lock".
    pub source: String,
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
        #[serde(default)]
        detected_hosts: Option<Vec<String>>,
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
    /// Approval subject + executable continuation: credential.request call blocked by
    /// network policy (e.g. localhost). After operator approval, runtime retries the same
    /// request with `approval_ref` and executes it.
    CredentialRequest {
        credential_id: String,
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        body: Option<serde_json::Value>,
        #[serde(default)]
        inject_secret_as: Option<String>,
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
    /// Approval subject only: "this approval request is for sharing a user profile with an agent."
    /// Not executed by the scheduler; the operator approves/denies, then the caller creates the binding.
    ProfileShare {
        user_id: String,
        agent_id: String,
        scope: String,
    },
    /// Approval subject only: "agent is stuck and needs human guidance."
    /// Not executed by the scheduler; once approved, the operator's guidance is injected
    /// as a system message and the session resumes.
    SessionEscalate {
        session_id: String,
        root_session_id: String,
        requested_by_agent_id: String,
        reason: String,
        context: String,
        urgency: String,
        suggested_actions: Vec<String>,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Approval subject only: sandbox.exec is about to mount layers whose build-time
    /// network scope is not covered by the current session's approval grants.
    /// Not executed by the scheduler; once approved, the caller retries sandbox.exec
    /// with this approval_ref — the approved LayerMount ref also authorises execution.
    LayerMount {
        /// Layers that require approval, with their build-time scope delta.
        layers: Vec<LayerMountScopeInfo>,
        /// The sandbox command this mount is for (context only).
        command: String,
    },
    /// Approval subject only: an `agent_revision_promote` would broaden the agent's
    /// capability set relative to the currently-active revision (R++2). The operator
    /// must explicitly acknowledge each added or broadened capability by name when
    /// approving. Not executed by the scheduler; once approved with the matching
    /// acknowledgement, the caller retries `agent_revision_promote` with
    /// `approval_ref` and the gate is bypassed.
    RevisionPromote {
        /// Agent whose alias is being promoted.
        agent_id: String,
        /// Incoming revision ID (the one that would become active).
        revision_id: String,
        /// Currently-active revision (the one being replaced).
        outgoing_revision_id: String,
        /// Capability type names that are present on the new revision but absent
        /// from the outgoing one (e.g. `["NetworkAccess"]`).
        added_capabilities: Vec<String>,
        /// Capability type names whose scope was broadened (wider hosts, scopes,
        /// patterns, larger spawn budget, etc.). Names only — full structured
        /// detail lives in `payload.broadened`.
        broadened_capabilities: Vec<String>,
        /// Full structured delta (for renderers / audit) — `{ added: [...],
        /// broadened: [{ capability_type, previous_scope, new_scope }, ...] }`.
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
            Self::AgentInstall { .. }
                | Self::CredentialPrompt { .. }
                | Self::SessionContinue { .. }
                | Self::ProfileShare { .. }
                | Self::SessionEscalate { .. }
                | Self::LayerMount { .. }
                | Self::RevisionPromote { .. }
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
            | Self::CredentialRequest { .. }
            | Self::SessionContinue { .. }
            | Self::ProfileShare { .. }
            | Self::SessionEscalate { .. }
            | Self::LayerMount { .. }
            | Self::RevisionPromote { .. } => true,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::WriteFile { .. } => "write_file",
            Self::SandboxExec { .. } => "sandbox_exec",
            Self::AgentInstall { .. } => "agent_install",
            Self::CredentialPrompt { .. } => "credential_prompt",
            Self::CredentialRequest { .. } => "credential_request",
            Self::SessionContinue { .. } => "session_continue",
            Self::ProfileShare { .. } => "profile_share",
            Self::SessionEscalate { .. } => "session_escalate",
            Self::LayerMount { .. } => "layer_mount",
            Self::RevisionPromote { .. } => "revision_promote",
        }
    }

    pub fn evidence_ref(&self) -> Option<String> {
        match self {
            Self::WriteFile { evidence_ref, .. } => evidence_ref.clone(),
            Self::SandboxExec { evidence_ref, .. } => evidence_ref.clone(),
            Self::AgentInstall { .. }
            | Self::CredentialPrompt { .. }
            | Self::CredentialRequest { .. }
            | Self::SessionContinue { .. }
            | Self::ProfileShare { .. }
            | Self::SessionEscalate { .. }
            | Self::LayerMount { .. }
            | Self::RevisionPromote { .. } => None,
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
            | Self::CredentialRequest { .. }
            | Self::SessionContinue { .. }
            | Self::ProfileShare { .. }
            | Self::SessionEscalate { .. }
            | Self::LayerMount { .. }
            | Self::RevisionPromote { .. } => {}
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Operator/decider's guidance note set at decision time (distinct from the
    /// agent's original `reason`). Persisted via `decision_reason` column.
    #[serde(default)]
    pub decision_reason: Option<String>,
    /// Required approval level for this request (operator, admin, agent:xyz).
    /// Defaults to Operator. Set by the gateway based on config escalation rules.
    #[serde(default)]
    pub approval_level: ApprovalLevel,
    #[serde(default)]
    pub similar_to_request_id: Option<String>,
    #[serde(default)]
    pub similarity_score: Option<f64>,
    #[serde(default)]
    pub min_dwell_ms: Option<i64>,
    #[serde(default)]
    pub confirm_phrase: Option<String>,
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
            reason: self.decision_reason.or(self.reason),
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

// ---------------------------------------------------------------------------
// Grant scope & targets (Phase 2 — approval hardening)
// ---------------------------------------------------------------------------

/// Scope of a session approval grant.
///
/// `RootSession` (default): the grant covers all children/siblings under the
/// root session — the current behaviour.  `Session`: the grant is limited to
/// the specific child session that was active when the approval was decided.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    #[default]
    RootSession,
    Session,
}

impl GrantScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RootSession => "root_session",
            Self::Session => "session",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "session" => Self::Session,
            _ => Self::RootSession,
        }
    }
}

/// A structured grant target describing what network host/path a grant covers.
///
/// Each approved `SandboxExec` produces one or more grant targets.  By default
/// these are `ExactHost` entries derived from the detected hosts, preserving
/// current behaviour.  Operators can narrow at approval time via `--target`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum GrantTarget {
    /// Exact hostname match, e.g. `"api.github.com"`.
    ExactHost(String),
    /// Matches any subdomain of the suffix, e.g. `"*.github.com"` matches
    /// `api.github.com` but NOT `github.com.evil.example`.
    HostSuffix(String),
    /// Exact host + port, e.g. `"api.github.com:443"`.
    HostAndPort { host: String, port: u16 },
    /// Matches URLs starting with this prefix, e.g.
    /// `"https://api.github.com/public/"`.
    UrlPrefix(String),
}

impl GrantTarget {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::ExactHost(_) => "exact_host",
            Self::HostSuffix(_) => "host_suffix",
            Self::HostAndPort { .. } => "host_and_port",
            Self::UrlPrefix(_) => "url_prefix",
        }
    }

    /// Check whether a request target (lowercased host or full URL) is covered
    /// by this grant target.
    pub fn matches(&self, request_target: &str) -> bool {
        match self {
            Self::ExactHost(host) => request_target.eq_ignore_ascii_case(host),
            Self::HostSuffix(suffix) => {
                let suffix = suffix.trim_start_matches("*.");
                let request = request_target.to_ascii_lowercase();
                let suffix = suffix.to_ascii_lowercase();
                if request == suffix {
                    return true;
                }
                request.ends_with(&format!(".{}", suffix))
            }
            Self::HostAndPort { host, port } => {
                let expected = format!("{}:{}", host.to_ascii_lowercase(), port);
                request_target.eq_ignore_ascii_case(&expected)
            }
            Self::UrlPrefix(prefix) => {
                fn lower_authority(url: &str) -> std::borrow::Cow<'_, str> {
                    let scheme_end = url.find("://").map(|p| p + 3).unwrap_or(0);
                    let rest = &url[scheme_end..];
                    if let Some(pos) = rest.find('/') {
                        let authority = &rest[..pos];
                        let path = &rest[pos..];
                        if authority.chars().any(|c| c.is_ascii_uppercase()) {
                            format!(
                                "{}{}{}",
                                &url[..scheme_end].to_ascii_lowercase(),
                                authority.to_ascii_lowercase(),
                                path
                            )
                            .into()
                        } else {
                            std::borrow::Cow::Borrowed(url)
                        }
                    } else {
                        url.to_ascii_lowercase().into()
                    }
                }
                let norm_req = lower_authority(request_target);
                let norm_pre = lower_authority(prefix);
                norm_req.starts_with(&*norm_pre)
            }
        }
    }
}

/// A structured grant row returned from the store for display and matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionApprovalGrant {
    pub id: i64,
    pub root_session_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub scope: GrantScope,
    pub granted_by: String,
    pub granted_at: String,
    pub source_approval_id: Option<String>,
    pub expires_at: Option<String>,
    pub targets: Vec<GrantTarget>,
}
