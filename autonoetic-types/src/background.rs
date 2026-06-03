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
    /// Approval subject + executable continuation: web.fetch blocked by network policy.
    /// After operator approval, runtime retries the same request with `approval_ref`.
    WebFetch {
        url: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        max_chars: Option<usize>,
        /// Concrete host targets inferred from `url` for session grants.
        #[serde(default)]
        detected_hosts: Option<Vec<String>>,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Approval subject + executable continuation: web.call blocked by network policy.
    /// After operator approval, runtime retries the same request with `approval_ref`.
    WebCall {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: Option<std::collections::HashMap<String, String>>,
        #[serde(default)]
        body: Option<serde_json::Value>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        max_chars: Option<usize>,
        /// Concrete host targets inferred from `url` for session grants.
        #[serde(default)]
        detected_hosts: Option<Vec<String>>,
        #[serde(default)]
        payload: Option<serde_json::Value>,
    },
    /// Approval subject + executable continuation: web.search blocked by network policy.
    /// After operator approval, runtime retries the same request with `approval_ref`.
    WebSearch {
        query: String,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        engine_url: Option<String>,
        #[serde(default)]
        duckduckgo_engine_url: Option<String>,
        #[serde(default)]
        google_engine_url: Option<String>,
        #[serde(default)]
        google_engine_id: Option<String>,
        #[serde(default)]
        google_api_key_env: Option<String>,
        #[serde(default)]
        google_engine_id_env: Option<String>,
        #[serde(default)]
        cache_ttl_secs: Option<u64>,
        /// Concrete host targets inferred from the resolved engine URL for session grants.
        #[serde(default)]
        detected_hosts: Option<Vec<String>>,
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
    /// capability set relative to the currently-active revision (P-2.16). The operator
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
            | Self::WebFetch { .. }
            | Self::WebCall { .. }
            | Self::WebSearch { .. }
            | Self::SessionContinue { .. }
            | Self::ProfileShare { .. }
            | Self::SessionEscalate { .. }
            | Self::LayerMount { .. }
            | Self::RevisionPromote { .. } => true,
        }
    }

    /// Concrete network host targets for session approval grants after operator approval.
    ///
    /// Returns `None` when the action has no grant-relevant hosts (e.g. install prompts).
    /// Prefers structured `detected_hosts` on variants that carry it; parses `CredentialRequest.url`
    /// otherwise.
    pub fn detected_hosts(&self) -> Option<Vec<String>> {
        match self {
            Self::SandboxExec { detected_hosts, .. }
            | Self::WebFetch { detected_hosts, .. }
            | Self::WebCall { detected_hosts, .. }
            | Self::WebSearch { detected_hosts, .. } => detected_hosts.clone(),
            Self::CredentialRequest { url: request_url, .. } => url::Url::parse(request_url)
                .ok()
                .and_then(|u| u.host_str().map(|h| vec![h.to_string()])),
            _ => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::WriteFile { .. } => "write_file",
            Self::SandboxExec { .. } => "sandbox_exec",
            Self::AgentInstall { .. } => "agent_install",
            Self::CredentialPrompt { .. } => "credential_prompt",
            Self::CredentialRequest { .. } => "credential_request",
            Self::WebFetch { .. } => "web_fetch",
            Self::WebCall { .. } => "web_call",
            Self::WebSearch { .. } => "web_search",
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
            | Self::WebFetch { .. }
            | Self::WebCall { .. }
            | Self::WebSearch { .. }
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
            | Self::WebFetch { .. }
            | Self::WebCall { .. }
            | Self::WebSearch { .. }
            | Self::SessionContinue { .. }
            | Self::ProfileShare { .. }
            | Self::SessionEscalate { .. }
            | Self::LayerMount { .. }
            | Self::RevisionPromote { .. } => {}
        }
        self
    }

    const REDACTED: &'static str = "***REDACTED***";

    fn redact_headers(headers: &std::collections::HashMap<String, String>, viewer: super::disclosure::ViewerClass) -> std::collections::HashMap<String, String> {
        match viewer {
            super::disclosure::ViewerClass::Admin => headers.clone(),
            _ => {
                let mut out = std::collections::HashMap::new();
                for (k, v) in headers {
                    if super::redaction::is_sensitive_key(k)
                        || super::redaction::looks_like_secret_value(v)
                    {
                        out.insert(k.clone(), Self::REDACTED.to_string());
                    } else {
                        out.insert(k.clone(), v.clone());
                    }
                }
                out
            }
        }
    }

    fn redact_json_value(value: &serde_json::Value, viewer: super::disclosure::ViewerClass) -> serde_json::Value {
        match viewer {
            super::disclosure::ViewerClass::Admin => value.clone(),
            // Delegate to the canonical helper (issue #156). The previous
            // local copy did not perform in-place masking on string values,
            // wholesale-redacting any string for which `looks_like_secret_value`
            // returned true. The canonical version masks bearer tokens,
            // env-var assignments, and URL query secrets in place — better
            // for operator review — and falls back to wholesale redaction
            // only for shapes that can't be masked locally (PEM, raw `sk-…`).
            _ => super::redaction::redact_json_value(value),
        }
    }

    pub fn redact_for_viewer(&self, viewer: super::disclosure::ViewerClass) -> Self {
        match viewer {
            super::disclosure::ViewerClass::Admin => self.clone(),
            super::disclosure::ViewerClass::Operator => self.redact_for_operator(),
            super::disclosure::ViewerClass::Agent => self.redact_for_agent(),
        }
    }

    fn redact_for_operator(&self) -> Self {
        match self {
            Self::CredentialRequest {
                credential_id,
                url,
                method,
                headers,
                body,
                inject_secret_as,
                payload,
            } => Self::CredentialRequest {
                credential_id: credential_id.clone(),
                url: url.clone(),
                method: method.clone(),
                headers: headers.as_ref().map(|h| Self::redact_headers(h, super::disclosure::ViewerClass::Operator)),
                body: body.as_ref().map(|b| Self::redact_json_value(b, super::disclosure::ViewerClass::Operator)),
                inject_secret_as: inject_secret_as.clone(),
                payload: payload.as_ref().map(|p| Self::redact_json_value(p, super::disclosure::ViewerClass::Operator)),
            },
            other => other.clone(),
        }
    }

    fn redact_for_agent(&self) -> Self {
        match self {
            Self::CredentialRequest {
                credential_id,
                url,
                method,
                ..
            } => Self::CredentialRequest {
                credential_id: credential_id.clone(),
                url: url.clone(),
                method: method.clone(),
                headers: Some(std::collections::HashMap::new()),
                body: None,
                inject_secret_as: None,
                payload: None,
            },
            Self::SandboxExec {
                dependencies,
                requires_approval,
                detected_hosts,
                ..
            } => Self::SandboxExec {
                // Command is blanked for the Agent class because shell strings
                // routinely embed secrets — `Authorization: Bearer …`, env-var
                // assignments, URL query params. Consistent with the Agent
                // redaction of `ExecutionTraceRecord::command` (commit 7f8525d).
                // Approving agents retain shape via `detected_hosts`,
                // `dependencies`, and `requires_approval`; operators see the
                // raw command (Operator class is identity for SandboxExec).
                command: Self::REDACTED.to_string(),
                dependencies: dependencies.clone(),
                requires_approval: *requires_approval,
                evidence_ref: None,
                detected_hosts: detected_hosts.clone(),
            },
            Self::WriteFile {
                path,
                requires_approval,
                ..
            } => Self::WriteFile {
                path: path.clone(),
                content: Self::REDACTED.to_string(),
                requires_approval: *requires_approval,
                evidence_ref: None,
            },
            other => other.clone(),
        }
    }

    pub fn redact_for_display(&self) -> Self {
        self.redact_for_viewer(super::disclosure::ViewerClass::Operator)
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

/// A code excerpt from an artifact file shown in an approval card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeExcerpt {
    pub file_name: String,
    pub content: String,
    pub language: String,
    pub size_bytes: usize,
    pub truncated: bool,
    pub truncated_from_bytes: Option<usize>,
}

/// Risk summary derived from RemoteAccessAnalyzer + auditor record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskSummary {
    pub host_count: usize,
    pub protocol_mix: Vec<String>,
    pub dangerous_patterns: Vec<String>,
    pub auditor_verdict: Option<String>,
    pub auditor_findings_link: Option<String>,
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
    /// Code excerpts from the artifact being approved (Phase 1 operator inspection).
    /// Populated when the approval is for a sandbox/artifact exec with an artifact ref.
    /// Empty for approvals where no artifact is involved.
    #[serde(default)]
    pub code_excerpts: Option<Vec<CodeExcerpt>>,
    /// Risk summary derived from RemoteAccessAnalyzer + auditor promotion record.
    #[serde(default)]
    pub risk_summary: Option<RiskSummary>,
}

impl ApprovalRequest {
    /// Principal kind of whoever decided this gate (#359 P1.b / #361), derived
    /// from `decided_by`. `None` while pending or for executor-mechanical
    /// resolutions. Mirrors the persisted `decided_by_kind` column.
    pub fn decided_by_kind(&self) -> Option<crate::principal::PrincipalKind> {
        self.decided_by
            .as_deref()
            .and_then(crate::principal::decider_principal_kind)
    }

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

impl ApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Cancelled => "cancelled",
        }
    }
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
    /// Sentinel-detected critical trajectory divergence — non-blocking notification.
    DivergenceSentinel,
}

impl std::fmt::Display for UserInteractionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clarification => write!(f, "clarification"),
            Self::Decision => write!(f, "decision"),
            Self::Proposal => write!(f, "proposal"),
            Self::Confirmation => write!(f, "confirmation"),
            Self::DivergenceSentinel => write!(f, "divergence_sentinel"),
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
            Self::DivergenceSentinel => "divergence_sentinel",
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

// ─────────────────────────────────────────────────────────────────────────────
// Tests for `ScheduledAction::redact_for_viewer`.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod redaction_tests {
    use super::*;
    use crate::disclosure::ViewerClass;
    use std::collections::HashMap;

    /// Substrings that must never appear in any field of an Agent-class
    /// redaction output. Same vocabulary as the causal_chain tests.
    const SECRET_TOKENS: &[&str] = &[
        "Bearer eyJhbGc",
        "sk-test-12345",
        "AKIAIOSFODNN",
        "ghp_realtoken",
        "-----BEGIN PRIVATE KEY-----",
        "PASSWORD=hunter2",
        "verysecret",
    ];

    fn blob_for(action: &ScheduledAction, viewer: ViewerClass) -> String {
        let r = action.redact_for_viewer(viewer);
        serde_json::to_string(&r).unwrap_or_default()
    }

    fn credential_request_with_secrets() -> ScheduledAction {
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), "Bearer eyJhbGc.foo.bar".into());
        headers.insert("X-Custom".into(), "ordinary".into());
        ScheduledAction::CredentialRequest {
            credential_id: "github_token".into(),
            url: "https://api.github.com/user".into(),
            method: Some("GET".into()),
            headers: Some(headers),
            body: Some(serde_json::json!({
                "client_secret": "verysecret",
                "scope": "read:user",
            })),
            inject_secret_as: Some("Authorization".into()),
            payload: Some(serde_json::json!({
                "api_key": "sk-test-12345abcdefghij",
                "ok": true,
            })),
        }
    }

    /// SandboxExec fixture with a benign command — used by tests that
    /// exercise the structural-redaction outcome (evidence_ref clearing,
    /// detected_hosts preservation) without dragging secret-bearing
    /// command material into the assertions.
    fn sandbox_exec_benign() -> ScheduledAction {
        ScheduledAction::SandboxExec {
            command: "ls -la /tmp".into(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: Some("evidence_handle_xyz".into()),
            detected_hosts: Some(vec!["x.example.com".into()]),
        }
    }

    fn sandbox_exec_with_secret_bearing_command() -> ScheduledAction {
        ScheduledAction::SandboxExec {
            command: "curl -H 'Authorization: Bearer eyJhbGc.foo' https://x".into(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: Some("evidence_handle_xyz".into()),
            detected_hosts: Some(vec!["x.example.com".into()]),
        }
    }

    fn write_file_with_secrets() -> ScheduledAction {
        ScheduledAction::WriteFile {
            path: "/tmp/keys.txt".into(),
            content: "PASSWORD=hunter2\nAKIAIOSFODNN1234567X".into(),
            requires_approval: true,
            evidence_ref: Some("evidence_xyz".into()),
        }
    }

    fn agent_install_payload() -> ScheduledAction {
        ScheduledAction::AgentInstall {
            agent_id: "coder.default".into(),
            summary: "install coder".into(),
            requested_by_agent_id: "planner.default".into(),
            install_fingerprint: "fp_abc".into(),
            payload: Some(serde_json::json!({
                "secret_token": "verysecret",
                "ok": true,
            })),
        }
    }

    // ── Admin: identity ──────────────────────────────────────────────────

    #[test]
    fn admin_viewer_round_trips_credential_request() {
        let original = credential_request_with_secrets();
        let r = original.redact_for_viewer(ViewerClass::Admin);
        // Compare via JSON: PartialEq on ScheduledAction is derived but
        // HashMap equality is order-independent, so JSON is the safest check.
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::to_value(&original).unwrap(),
        );
    }

    #[test]
    fn admin_viewer_round_trips_sandbox_exec() {
        let original = sandbox_exec_benign();
        let r = original.redact_for_viewer(ViewerClass::Admin);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::to_value(&original).unwrap(),
        );
    }

    #[test]
    fn admin_viewer_round_trips_write_file() {
        let original = write_file_with_secrets();
        let r = original.redact_for_viewer(ViewerClass::Admin);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::to_value(&original).unwrap(),
        );
    }

    // ── Agent: maximum redaction ─────────────────────────────────────────

    #[test]
    fn agent_viewer_credential_request_strips_headers_body_payload() {
        let r = credential_request_with_secrets().redact_for_viewer(ViewerClass::Agent);
        match r {
            ScheduledAction::CredentialRequest {
                credential_id,
                url,
                method,
                headers,
                body,
                inject_secret_as,
                payload,
            } => {
                // Identifying / structural fields preserved.
                assert_eq!(credential_id, "github_token");
                assert_eq!(url, "https://api.github.com/user");
                assert_eq!(method.as_deref(), Some("GET"));
                // Headers blanked to an empty map; body / payload / inject blanked.
                assert!(headers.unwrap().is_empty(), "headers must be empty for Agent");
                assert!(body.is_none(), "body must be None for Agent");
                assert!(payload.is_none(), "payload must be None for Agent");
                assert!(inject_secret_as.is_none(), "inject_secret_as must be None for Agent");
            }
            other => panic!("expected CredentialRequest, got {other:?}"),
        }
    }

    #[test]
    fn agent_viewer_sandbox_exec_redacts_command_and_clears_evidence_ref() {
        let r = sandbox_exec_benign().redact_for_viewer(ViewerClass::Agent);
        match r {
            ScheduledAction::SandboxExec {
                command,
                evidence_ref,
                detected_hosts,
                requires_approval,
                ..
            } => {
                // Command is blanked for the Agent class (issue #158 fix).
                // Approving agents rely on detected_hosts / dependencies /
                // requires_approval for command shape, not the raw string.
                assert_eq!(command, ScheduledAction::REDACTED);
                // Evidence ref is cleared (would resolve to a content-store blob).
                assert_eq!(evidence_ref, None);
                // Detected hosts and approval flag preserved.
                assert!(detected_hosts.is_some());
                assert!(requires_approval);
            }
            other => panic!("expected SandboxExec, got {other:?}"),
        }
    }

    #[test]
    fn agent_viewer_sandbox_exec_does_not_leak_command_secrets() {
        // Issue #158 fix: a command embedding a Bearer token must NOT survive
        // Agent-class redaction. The command is replaced with "***REDACTED***"
        // wholesale, regardless of content.
        let r =
            sandbox_exec_with_secret_bearing_command().redact_for_viewer(ViewerClass::Agent);
        match r {
            ScheduledAction::SandboxExec { command, .. } => {
                assert_eq!(command, ScheduledAction::REDACTED);
                assert!(
                    !command.contains("Bearer"),
                    "regression: SandboxExec.command leaked Bearer prefix for Agent: {command}"
                );
                assert!(
                    !command.contains("eyJ"),
                    "regression: SandboxExec.command leaked JWT-like prefix for Agent: {command}"
                );
            }
            other => panic!("expected SandboxExec, got {other:?}"),
        }
    }

    #[test]
    fn operator_viewer_sandbox_exec_preserves_command() {
        // Operator class is identity for SandboxExec — the command is needed
        // for human approval review. Pinning this so a future change that
        // tightens Operator redaction doesn't accidentally hide commands from
        // operators.
        let original = sandbox_exec_with_secret_bearing_command();
        let r = original.redact_for_viewer(ViewerClass::Operator);
        match r {
            ScheduledAction::SandboxExec { command, .. } => {
                assert!(command.contains("Bearer"));
            }
            other => panic!("expected SandboxExec, got {other:?}"),
        }
    }

    #[test]
    fn agent_viewer_write_file_redacts_content_only() {
        let r = write_file_with_secrets().redact_for_viewer(ViewerClass::Agent);
        match r {
            ScheduledAction::WriteFile {
                path,
                content,
                requires_approval,
                evidence_ref,
            } => {
                // Path is visible (operationally needed); content is redacted;
                // evidence_ref cleared.
                assert_eq!(path, "/tmp/keys.txt");
                assert_eq!(content, ScheduledAction::REDACTED);
                assert!(requires_approval);
                assert_eq!(evidence_ref, None);
            }
            other => panic!("expected WriteFile, got {other:?}"),
        }
    }

    #[test]
    fn agent_viewer_falls_through_for_agent_install_today() {
        // Variants without explicit redaction in `redact_for_agent` fall through
        // to `other.clone()`. AgentInstall is one of them today; this pin will
        // FAIL if a future change exposes secrets via that path without explicit
        // handling, prompting the author to add a redact arm.
        let original = agent_install_payload();
        let r = original.redact_for_viewer(ViewerClass::Agent);
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::to_value(&original).unwrap(),
            "AgentInstall currently falls through unmodified — when this pin breaks, \
             explicitly redact the variant in ScheduledAction::redact_for_agent \
             rather than weakening the test"
        );
    }

    // ── Operator: targeted redaction in CredentialRequest ────────────────

    #[test]
    fn operator_viewer_credential_request_redacts_sensitive_headers() {
        let r = credential_request_with_secrets().redact_for_viewer(ViewerClass::Operator);
        match r {
            ScheduledAction::CredentialRequest {
                headers, body, payload, ..
            } => {
                let headers = headers.expect("headers preserved structurally");
                assert_eq!(
                    headers.get("Authorization").map(|s| s.as_str()),
                    Some(ScheduledAction::REDACTED),
                    "Authorization header must be redacted for Operator"
                );
                assert_eq!(
                    headers.get("X-Custom").map(|s| s.as_str()),
                    Some("ordinary"),
                    "non-sensitive header must survive: {headers:?}"
                );
                let body = body.expect("body preserved structurally");
                assert_eq!(body["client_secret"], "***REDACTED***");
                assert_eq!(body["scope"], "read:user");
                let payload = payload.expect("payload preserved structurally");
                assert_eq!(payload["api_key"], "***REDACTED***");
                assert_eq!(payload["ok"], true);
            }
            other => panic!("expected CredentialRequest, got {other:?}"),
        }
    }

    // ── Property: no secrets leak via Agent class ────────────────────────

    #[test]
    fn agent_viewer_no_secrets_leak_property() {
        // After #158, SandboxExec is included with a secret-bearing command —
        // the redaction must blank the command string entirely. AgentInstall
        // is excluded because it falls through unmodified today (pinned
        // separately above).
        let actions = [
            credential_request_with_secrets(),
            sandbox_exec_with_secret_bearing_command(),
            write_file_with_secrets(),
        ];
        for action in actions.iter() {
            let blob = blob_for(action, ViewerClass::Agent);
            for token in SECRET_TOKENS {
                assert!(
                    !blob.contains(token),
                    "Agent-class redacted {action:?} must not contain '{token}'\nblob: {blob}"
                );
            }
        }
    }

    // ── Helper visibility ────────────────────────────────────────────────

    #[test]
    fn looks_like_secret_value_recognises_documented_patterns() {
        use crate::redaction::looks_like_secret_value;
        assert!(looks_like_secret_value("Bearer eyJhbGc.foo"));
        assert!(looks_like_secret_value("sk-abc12345"));
        assert!(looks_like_secret_value("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(!looks_like_secret_value("plain text"));
        assert!(!looks_like_secret_value(""));
        assert!(!looks_like_secret_value("   "));
    }
}

#[cfg(test)]
mod detected_hosts_tests {
    use super::ScheduledAction;

    #[test]
    fn sandbox_exec_clones_detected_hosts() {
        let a = ScheduledAction::SandboxExec {
            command: "".into(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["a.example.com".into(), "b.example.com".into()]),
        };
        assert_eq!(
            a.detected_hosts(),
            Some(vec!["a.example.com".into(), "b.example.com".into()])
        );
    }

    #[test]
    fn sandbox_exec_none_when_no_hosts() {
        let a = ScheduledAction::SandboxExec {
            command: "".into(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        assert_eq!(a.detected_hosts(), None);
    }

    #[test]
    fn credential_request_parses_host_from_url() {
        let a = ScheduledAction::CredentialRequest {
            credential_id: "c".into(),
            url: "http://localhost:9876/skill.md".into(),
            method: Some("GET".into()),
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: None,
        };
        assert_eq!(a.detected_hosts(), Some(vec!["localhost".into()]));
    }

    #[test]
    fn non_network_actions_return_none() {
        let a = ScheduledAction::WriteFile {
            path: "/tmp/x".into(),
            content: "".into(),
            requires_approval: true,
            evidence_ref: None,
        };
        assert_eq!(a.detected_hosts(), None);
    }
}
