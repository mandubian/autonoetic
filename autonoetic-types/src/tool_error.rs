//! Tool error types for structured failure feedback.

use serde::{Deserialize, Serialize};

/// Gateway-owned mechanical classification for workflow-relevant failures.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    TransientInfra,
    ApprovalPending,
    AwaitingUserInput,
    Timeout,
    ChildCancelled,
    ArtifactInvalid,
    DependencyMissing,
    GateUnsatisfied,
    GateUnableToEvaluate,
    InstallConflict,
    PolicyDenied,
    SchemaValidationFailed,
    TaskContractInvalid,
    Unknown,
}

/// Gateway-owned retry guidance for workflow orchestration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    Wait,
    RetrySameStage,
    RetryAfterExternalRecovery,
    DoNotRetry,
    EscalateHuman,
    FixArtifactThenRetry,
}

/// Best-effort record of whether a failed stage may have committed side effects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectState {
    #[serde(rename = "none")]
    NoSideEffect,
    Committed,
    Unknown,
}

/// Type of tool error, indicating whether it's recoverable or fatal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorType {
    /// Validation error: malformed input, missing required field, policy denial.
    /// The agent can repair and retry.
    Validation,
    /// Permission error: agent lacks required capability or scope.
    /// The agent can request additional authorization or adjust scope.
    Permission,
    /// Resource error: missing file, unavailable service, rate limit.
    /// The agent can retry with backoff or use alternative.
    Resource,
    /// Execution error: tool ran but produced an unexpected result.
    /// The agent can inspect and adjust.
    Execution,
    /// Fatal error: corrupted state, invariant violation, unsafe condition.
    /// The agent session should abort; this is not recoverable.
    Fatal,
    /// Conflict error: duplicate entry, state conflict, concurrent modification.
    /// The agent should resolve the conflict and retry.
    Conflict,
    /// Quota exceeded: budget exhausted, rate limit hit, max attempts reached.
    /// The agent should wait or use an alternative path.
    QuotaExceeded,
    /// Not found: requested resource does not exist.
    /// The agent can create it or use an alternative.
    NotFound,
    /// Timeout: operation exceeded its time limit.
    /// The agent can retry with backoff.
    Timeout,
    SandboxUnavailable,
}

impl std::fmt::Display for ToolErrorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolErrorType::Validation => write!(f, "validation"),
            ToolErrorType::Permission => write!(f, "permission"),
            ToolErrorType::Resource => write!(f, "resource"),
            ToolErrorType::Execution => write!(f, "execution"),
            ToolErrorType::Fatal => write!(f, "fatal"),
            ToolErrorType::Conflict => write!(f, "conflict"),
            ToolErrorType::QuotaExceeded => write!(f, "quota_exceeded"),
            ToolErrorType::NotFound => write!(f, "not_found"),
            ToolErrorType::Timeout => write!(f, "timeout"),
            ToolErrorType::SandboxUnavailable => write!(f, "sandbox_unavailable"),
        }
    }
}

/// A lawful next move the agent can take from inside a denial itself:
/// propose an amendment, delegate to a capable agent, or inspect itself.
/// Static and pre-committed (Ri-0.3) — the gateway maps rule IDs to
/// affordances mechanically, it never judges which move is "best".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AvailableAction {
    /// Machine key: "propose_amendment" | "delegate" | "self_describe".
    pub action: String,
    /// One sentence, imperative.
    pub description: String,
    /// Exact tool name the agent can call, if one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Constitutional clause backing the affordance, e.g. "Ri-0.8".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clause: Option<String>,
    /// Capability type name required, e.g. "ConstitutionalProposal".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_capability: Option<String>,
}

/// Structured tool error response for agent feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolError {
    /// Always false for errors.
    #[serde(rename = "ok")]
    pub success: bool,
    /// Type of error indicating recoverability.
    pub error_type: ToolErrorType,
    /// Human-readable error message.
    pub message: String,
    /// Optional hint for repairing the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_hint: Option<String>,
    /// Optional original error details (for logging, not always exposed to agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Specific constitutional or policy rule IDs enforced for this error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enforced_rules: Vec<String>,
    /// Lawful next moves available to the agent from inside this denial
    /// (Ri-0.3 named rejection): propose an amendment, delegate, or inspect
    /// itself. Populated mechanically from `enforced_rules`; never judged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_actions: Vec<AvailableAction>,
    /// Mechanical failure classification used by workflow orchestration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Gateway retry guidance for the current workflow stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_advice: Option<RetryAdvice>,
    /// Normalized recoverability hint for legacy consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// True when progress depends on an external approval/input/event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_external_event: Option<bool>,
    /// True when the failure should escalate to a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_human: Option<bool>,
    /// Whether the failing stage may already have committed side effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect_state: Option<SideEffectState>,
    /// Stable dedupe key for durable operations when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Stable, machine-readable failure code (snake_case), e.g.
    /// `auditor_pass_missing`. Finer-grained than `error_type` so an
    /// orchestrator branches on one field instead of parsing `message` prose.
    /// Optional and additive (P-5.11); serialized as `error`.
    #[serde(rename = "error", default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ToolError {
    fn new(
        error_type: ToolErrorType,
        message: impl Into<String>,
        repair_hint: Option<String>,
        details: Option<String>,
    ) -> Self {
        Self {
            success: false,
            error_type,
            message: message.into(),
            repair_hint,
            details,
            enforced_rules: Vec::new(),
            available_actions: Vec::new(),
            failure_class: None,
            retry_advice: None,
            retryable: None,
            requires_external_event: None,
            requires_human: None,
            side_effect_state: None,
            dedupe_key: None,
            code: None,
        }
    }

    /// Attach the stable machine-readable failure `code` (P-5.11).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Override the `repair_hint` (the mechanical remedy). Useful with
    /// constructors like `permission` that only take a message.
    pub fn with_repair_hint(mut self, repair_hint: impl Into<String>) -> Self {
        self.repair_hint = Some(repair_hint.into());
        self
    }

    /// Creates a new validation error.
    pub fn validation(message: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Validation,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    /// Creates a new permission error.
    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(
            ToolErrorType::Permission,
            message,
            Some(
                "Request additional authorization or adjust the scope of your request.".to_string(),
            ),
            None,
        )
    }

    /// Creates a new resource error.
    pub fn resource(message: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Resource,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    /// Creates a new execution error.
    pub fn execution(message: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Execution,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    /// Creates a new fatal error.
    pub fn fatal(message: impl Into<String>, details: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Fatal,
            message,
            None,
            details.map(|d| d.into()),
        )
    }

    /// Creates a new conflict error.
    pub fn conflict(message: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Conflict,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    pub fn with_enforced_rules(mut self, enforced_rules: Vec<String>) -> Self {
        self.enforced_rules = enforced_rules;
        self
    }

    pub fn with_available_actions(mut self, actions: Vec<AvailableAction>) -> Self {
        self.available_actions = actions;
        self
    }

    pub fn with_failure_class(mut self, failure_class: FailureClass) -> Self {
        self.failure_class = Some(failure_class);
        self
    }

    pub fn with_retry_advice(mut self, retry_advice: RetryAdvice) -> Self {
        self.retry_advice = Some(retry_advice);
        self
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = Some(retryable);
        self
    }

    pub fn with_requires_external_event(mut self, requires_external_event: bool) -> Self {
        self.requires_external_event = Some(requires_external_event);
        self
    }

    pub fn with_requires_human(mut self, requires_human: bool) -> Self {
        self.requires_human = Some(requires_human);
        self
    }

    pub fn with_side_effect_state(mut self, side_effect_state: SideEffectState) -> Self {
        self.side_effect_state = Some(side_effect_state);
        self
    }

    pub fn with_dedupe_key(mut self, dedupe_key: impl Into<String>) -> Self {
        self.dedupe_key = Some(dedupe_key.into());
        self
    }

    /// Creates a new quota exceeded error.
    pub fn quota_exceeded(
        message: impl Into<String>,
        repair_hint: Option<impl Into<String>>,
    ) -> Self {
        Self::new(
            ToolErrorType::QuotaExceeded,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    /// Creates a new not found error.
    pub fn not_found(resource: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::NotFound,
            format!("{} not found", resource.into()),
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    /// Creates a new timeout error.
    pub fn timeout(message: impl Into<String>, repair_hint: Option<impl Into<String>>) -> Self {
        Self::new(
            ToolErrorType::Timeout,
            message,
            repair_hint.map(|h| h.into()),
            None,
        )
    }

    pub fn sandbox_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            ToolErrorType::SandboxUnavailable,
            message,
            Some("Install the sandbox driver on this host or use a different agent/sandbox backend.".to_string()),
            None,
        )
    }

    /// Returns true if this error is recoverable (agent can retry).
    pub fn is_recoverable(&self) -> bool {
        !matches!(self.error_type, ToolErrorType::Fatal)
    }

    /// Converts the error to a JSON string for tool_result.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(
                r#"{{"ok":false,"error_type":"fatal","message":"Failed to serialize error: {}"}}"#,
                e
            )
        })
    }

    /// Creates a JSON response with ok=false for tool execution failure.
    pub fn to_error_response(&self) -> String {
        self.to_json_string()
    }
}

/// Helper macro to return a structured error from a tool's execute method.
///
/// Usage:
/// ```ignore
/// return tool_error!(validation, "missing field 'id'", "Include an 'id' field in your request");
/// return tool_error!(permission, "NetworkAccess required for host api.example.com");
/// return tool_error!(not_found, "agent '{}' not found", agent_id);
/// ```
#[macro_export]
macro_rules! tool_error {
    (validation, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::validation($msg, Some($hint)).to_error_response());
    }};
    (validation, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::validation($msg, None::<String>).to_error_response(),
        );
    }};
    (permission, $msg:expr) => {{
        return Ok($crate::tool_error::ToolError::permission($msg).to_error_response());
    }};
    (resource, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::resource($msg, Some($hint)).to_error_response());
    }};
    (resource, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::resource($msg, None::<String>).to_error_response(),
        );
    }};
    (execution, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::execution($msg, Some($hint)).to_error_response());
    }};
    (execution, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::execution($msg, None::<String>).to_error_response(),
        );
    }};
    (fatal, $msg:expr, $details:expr) => {{
        return Ok($crate::tool_error::ToolError::fatal($msg, Some($details)).to_error_response());
    }};
    (fatal, $msg:expr) => {{
        return Ok($crate::tool_error::ToolError::fatal($msg, None::<String>).to_error_response());
    }};
    (conflict, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::conflict($msg, Some($hint)).to_error_response());
    }};
    (conflict, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::conflict($msg, None::<String>).to_error_response(),
        );
    }};
    (quota_exceeded, $msg:expr, $hint:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::quota_exceeded($msg, Some($hint)).to_error_response(),
        );
    }};
    (quota_exceeded, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::quota_exceeded($msg, None::<String>).to_error_response(),
        );
    }};
    (not_found, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::not_found($msg, Some($hint)).to_error_response());
    }};
    (not_found, $msg:expr) => {{
        return Ok(
            $crate::tool_error::ToolError::not_found($msg, None::<String>).to_error_response(),
        );
    }};
    (timeout, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::timeout($msg, Some($hint)).to_error_response());
    }};
    (timeout, $msg:expr) => {{
        return Ok($crate::tool_error::ToolError::timeout($msg, None::<String>).to_error_response());
    }};
    (sandbox_unavailable, $msg:expr, $hint:expr) => {{
        return Ok($crate::tool_error::ToolError::sandbox_unavailable($msg).with_repair_hint($hint).to_error_response());
    }};
    (sandbox_unavailable, $msg:expr) => {{
        return Ok($crate::tool_error::ToolError::sandbox_unavailable($msg).to_error_response());
    }};
}

/// Helper macro to return a structured error from a tool's execute method using anyhow.
///
/// Usage:
/// ```ignore
/// return tool_error_tagged!(validation, anyhow::anyhow!("invalid JSON"));
/// return tool_error_tagged!(permission, anyhow::anyhow!("denied"));
/// ```
#[macro_export]
macro_rules! tool_error_tagged {
    ($variant:ident, $err:expr) => {{
        let tagged = $crate::tool_error::tagged::Tagged::$variant($err);
        let err: $crate::tool_error::ToolError = tagged.into();
        return Ok(err.to_error_response());
    }};
}

/// Helper to create tagged errors with explicit error type classification.
/// Use these functions instead of anyhow::anyhow! for tool errors to ensure
/// proper classification without relying on string heuristics.
pub mod tagged {
    use super::*;
    use std::error::Error;

    /// A wrapper that attaches error type metadata to an anyhow::Error.
    #[derive(Debug)]
    pub struct Tagged {
        error_type: ToolErrorType,
        source: anyhow::Error,
        enforced_rules: Vec<String>,
    }

    // SAFETY: Tagged is safe to send across thread boundaries because:
    // - The inner anyhow::Error is wrapped in a concrete owned type with no interior mutability
    // - The error_type field is Clone + Send + Sync (ToolErrorType derives both)
    // - No references are held that could become invalid across threads
    unsafe impl Send for Tagged {}
    unsafe impl Sync for Tagged {}

    impl Tagged {
        pub fn validation(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Validation,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn permission(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Permission,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn permission_with_rules(
            err: impl Into<anyhow::Error>,
            enforced_rules: Vec<String>,
        ) -> Self {
            Self {
                error_type: ToolErrorType::Permission,
                source: err.into(),
                enforced_rules,
            }
        }

        pub fn resource(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Resource,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn execution(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Execution,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn fatal(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Fatal,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn conflict(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Conflict,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn quota_exceeded(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::QuotaExceeded,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn not_found(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::NotFound,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }

        pub fn timeout(err: impl Into<anyhow::Error>) -> Self {
            Self {
                error_type: ToolErrorType::Timeout,
                source: err.into(),
                enforced_rules: Vec::new(),
            }
        }
    }

    impl std::fmt::Display for Tagged {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.error_type, self.source)
        }
    }

    impl Error for Tagged {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    impl Tagged {
        /// The constitutional rule/right IDs this refusal enforces (if any).
        /// Lets a boundary (e.g. the JSON-RPC layer) surface them to clients
        /// without consuming the error.
        pub fn enforced_rules(&self) -> &[String] {
            &self.enforced_rules
        }

        /// Extracts the error type and message from this tagged error.
        pub fn into_parts(self) -> (ToolErrorType, String, Vec<String>) {
            (
                self.error_type.clone(),
                self.source.to_string(),
                self.enforced_rules,
            )
        }
    }
}

impl From<tagged::Tagged> for ToolError {
    fn from(tagged: tagged::Tagged) -> Self {
        let (error_type, message, enforced_rules) = tagged.into_parts();
        let err = match error_type {
            ToolErrorType::Validation => Self::validation(message, None::<String>),
            ToolErrorType::Permission => Self::permission(message),
            ToolErrorType::Resource => Self::resource(message, None::<String>),
            ToolErrorType::Execution => Self::execution(message, None::<String>),
            ToolErrorType::Fatal => Self::fatal(message.clone(), Some(message)),
            ToolErrorType::Conflict => Self::conflict(message, None::<String>),
            ToolErrorType::QuotaExceeded => Self::quota_exceeded(message, None::<String>),
            ToolErrorType::NotFound => Self::not_found(message, None::<String>),
            ToolErrorType::Timeout => Self::timeout(message, None::<String>),
            ToolErrorType::SandboxUnavailable => Self::sandbox_unavailable(message),
        };
        err.with_enforced_rules(enforced_rules)
    }
}

impl From<anyhow::Error> for ToolError {
    fn from(err: anyhow::Error) -> Self {
        fn derive_validation_hint(msg: &str) -> Option<String> {
            let lower = msg.to_ascii_lowercase();

            if lower.contains("invalid json arguments for 'agent.install'")
                && lower.contains("missing field `type`")
            {
                return Some("agent.install.capabilities items must include a valid `type` field (for example `NetConnect`, `ReadAccess`, `WriteAccess`, `ShellExec`, `AgentSpawn`).".to_string());
            }

            if lower.contains("invalid json arguments for 'agent.install'")
                && lower.contains("unknown variant")
                && lower.contains("expected one of")
            {
                return Some("agent.install.capabilities[].type must match one allowed enum exactly. Use one of the values listed in the error and retry.".to_string());
            }

            if lower.contains("invalid json arguments for 'agent.install'")
                && lower.contains("missing field")
            {
                return Some("agent.install payload is missing required fields. Re-check required keys and ensure nested capability objects are complete.".to_string());
            }

            if lower.contains("must not be empty") {
                return Some("Ensure all required fields are provided and not empty.".to_string());
            }

            if lower.contains("invalid json arguments")
                && lower.contains("capabilities[")
                && lower.contains("missing field")
            {
                return Some("capabilities items are tagged objects and each type requires specific fields: \
                    SandboxFunctions→allowed[], NetworkAccess→hosts[], \
                    ReadAccess/WriteAccess/UserProfileAccess→scopes[], \
                    AgentSpawn→max_children, BackgroundReevaluation→min_interval_secs+allow_reasoning, \
                    PromoteWith→capabilities[]. Add the missing field named in the error and retry.".to_string());
            }

            if lower.contains("invalid json") {
                return Some("Check the tool schema and ensure JSON is valid.".to_string());
            }

            None
        }

        // Check if this is a tagged error by looking at the error chain
        for cause in err.chain() {
            let msg = cause.to_string();
            let msg_trimmed = msg.trim();
            if msg.starts_with("validation:") {
                let inner = msg.strip_prefix("validation:").unwrap_or(&msg).trim();
                let repair_hint = derive_validation_hint(msg_trimmed);
                return Self::validation(inner.to_string(), repair_hint);
            } else if msg.starts_with("permission:") {
                let inner = msg.strip_prefix("permission:").unwrap_or(&msg).trim();
                return Self::permission(inner.to_string());
            } else if msg.starts_with("resource:") {
                let inner = msg.strip_prefix("resource:").unwrap_or(&msg).trim();
                return Self::resource(inner.to_string(), None::<String>);
            } else if msg.starts_with("execution:") {
                let inner = msg.strip_prefix("execution:").unwrap_or(&msg).trim();
                return Self::execution(inner.to_string(), None::<String>);
            } else if msg.starts_with("fatal:") {
                let inner = msg.strip_prefix("fatal:").unwrap_or(&msg).trim();
                return Self::fatal(inner.to_string(), Some(err.to_string()));
            } else if msg.starts_with("conflict:") {
                let inner = msg.strip_prefix("conflict:").unwrap_or(&msg).trim();
                return Self::conflict(inner.to_string(), None::<String>);
            } else if msg.starts_with("quota_exceeded:") {
                let inner = msg.strip_prefix("quota_exceeded:").unwrap_or(&msg).trim();
                return Self::quota_exceeded(inner.to_string(), None::<String>);
            } else if msg.starts_with("not_found:") {
                let inner = msg.strip_prefix("not_found:").unwrap_or(&msg).trim();
                return Self::not_found(inner.to_string(), None::<String>);
            } else if msg.starts_with("timeout:") {
                let inner = msg.strip_prefix("timeout:").unwrap_or(&msg).trim();
                return Self::timeout(inner.to_string(), None::<String>);
            } else if msg.starts_with("sandbox_unavailable:") {
                let inner = msg.strip_prefix("sandbox_unavailable:").unwrap_or(&msg).trim();
                return Self::sandbox_unavailable(inner.to_string());
            }
        }

        // Fall back to string-based classification for untagged errors
        let msg = err.to_string();
        let msg_trimmed = msg.trim();
        if msg.contains("policy") || msg.contains("Permission Denied") || msg.contains("denied") {
            Self::permission(msg)
        } else if msg_trimmed.contains("must not be empty")
            || msg_trimmed.contains("Invalid")
            || msg_trimmed.contains("must")
            || msg_trimmed.contains("required")
            || msg_trimmed.contains("denied by policy")
        {
            let repair_hint = derive_validation_hint(msg_trimmed).unwrap_or_else(|| {
                "Check the tool schema and ensure all required fields are provided with valid values."
                    .to_string()
            });
            Self::validation(msg, Some(repair_hint))
        } else if msg_trimmed.contains("timeout") {
            Self::timeout(msg, Some("The operation timed out. Retry with backoff."))
        } else if msg_trimmed.contains("not found")
            || msg_trimmed.contains("File not found")
            || msg_trimmed.contains("connection")
        {
            Self::resource(msg, Some("Verify the resource exists or try again later."))
        } else if msg_trimmed.contains("corrupted")
            || msg_trimmed.contains("invariant")
            || msg_trimmed.contains("unsafe")
            || msg_trimmed.contains("Unknown tool")
        {
            Self::fatal(msg, Some(err.to_string()))
        } else {
            // Default to execution error for unknown types
            Self::execution(msg, None::<String>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_error() {
        let err = ToolError::validation(
            "missing field 'id'",
            Some("Include an 'id' field in your request"),
        );
        assert!(!err.success);
        assert_eq!(err.error_type, ToolErrorType::Validation);
        assert!(err.is_recoverable());
        assert!(err.repair_hint.is_some());
    }

    // Tagged anyhow errors use the "{type}: {msg}" convention (Tagged::Display).
    // Stripping the prefix must not leave a leading space on the surfaced
    // message (PR #601 review).
    #[test]
    fn tagged_anyhow_message_has_no_leading_space() {
        let err: ToolError = anyhow::anyhow!("resource: sandbox driver 'bwrap' not found").into();
        assert_eq!(err.error_type, ToolErrorType::Resource);
        assert_eq!(err.message, "sandbox driver 'bwrap' not found");
        assert!(!err.message.starts_with(' '));
    }

    #[test]
    fn sandbox_unavailable_prefix_maps_to_typed_error() {
        let err: ToolError = anyhow::anyhow!("sandbox_unavailable: sandbox driver 'bwrap' not found on PATH — ... [sandbox_driver_unavailable]").into();
        assert_eq!(err.error_type, ToolErrorType::SandboxUnavailable);
        assert_eq!(err.message, "sandbox driver 'bwrap' not found on PATH — ... [sandbox_driver_unavailable]");
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_fatal_error() {
        let err = ToolError::fatal("corrupted state", Some("state hash mismatch"));
        assert!(!err.success);
        assert_eq!(err.error_type, ToolErrorType::Fatal);
        assert!(!err.is_recoverable());
        assert!(err.repair_hint.is_none());
    }

    #[test]
    fn test_error_to_json() {
        let err = ToolError::validation("bad input", Some("fix it"));
        let json = err.to_json_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get("ok").unwrap(), false);
        assert_eq!(parsed.get("error_type").unwrap(), "validation");
    }

    #[test]
    fn test_anyhow_conversion() {
        let anyhow_err = anyhow::anyhow!("memory read denied by policy");
        let err: ToolError = anyhow_err.into();
        assert_eq!(err.error_type, ToolErrorType::Permission);
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_validation_conversion() {
        let anyhow_err = anyhow::anyhow!("id must not be empty");
        let err: ToolError = anyhow_err.into();
        assert_eq!(err.error_type, ToolErrorType::Validation);
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_timeout_classification_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("request timeout while contacting upstream service");
        let err: ToolError = anyhow_err.into();
        assert_eq!(err.error_type, ToolErrorType::Timeout);
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_agent_install_missing_type_gets_specific_hint() {
        let anyhow_err = anyhow::anyhow!(
            "Invalid JSON arguments for 'agent.install': missing field `type` at line 1 column 123"
        );
        let err: ToolError = anyhow_err.into();
        assert_eq!(err.error_type, ToolErrorType::Validation);
        let hint = err.repair_hint.unwrap_or_default();
        assert!(hint.contains("capabilities"));
        assert!(hint.contains("type"));
    }

    #[test]
    fn test_agent_install_unknown_variant_gets_specific_hint() {
        let anyhow_err = anyhow::anyhow!(
            "Invalid JSON arguments for 'agent.install': unknown variant `capability`, expected one of `ToolInvoke`, `ReadAccess` at line 1 column 42"
        );
        let err: ToolError = anyhow_err.into();
        assert_eq!(err.error_type, ToolErrorType::Validation);
        let hint = err.repair_hint.unwrap_or_default();
        assert!(hint.contains("allowed enum"));
    }

    #[test]
    fn with_code_serializes_as_error_field_and_is_omitted_when_absent() {
        // Present → serialized under the canonical `error` key (P-5.11 stable code).
        let coded = ToolError::permission("auditor did not pass").with_code("auditor_pass_missing");
        let v: serde_json::Value = serde_json::from_str(&coded.to_json_string()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error_type"], "permission");
        assert_eq!(v["error"], "auditor_pass_missing");

        // Absent → field omitted, so the base envelope is unchanged for callers
        // that do not set a code (additive, P-5.11).
        let plain = ToolError::permission("denied");
        let v2: serde_json::Value = serde_json::from_str(&plain.to_json_string()).unwrap();
        assert!(v2.get("error").is_none(), "error code omitted when absent: {v2}");
    }

    #[test]
    fn available_actions_serialize_when_present_and_omitted_when_absent() {
        let action = AvailableAction {
            action: "propose_amendment".to_string(),
            description: "propose an amendment".to_string(),
            tool: Some("constitution_propose_amendment".to_string()),
            clause: Some("Ri-0.8".to_string()),
            requires_capability: Some("ConstitutionalProposal".to_string()),
        };
        let with_actions = ToolError::permission("denied").with_available_actions(vec![action]);
        let v: serde_json::Value = serde_json::from_str(&with_actions.to_json_string()).unwrap();
        let actions = v["available_actions"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["action"], "propose_amendment");
        assert_eq!(actions[0]["clause"], "Ri-0.8");

        // Absent → key omitted entirely (additive, mirrors enforced_rules).
        let without_actions = ToolError::permission("denied");
        let v2: serde_json::Value =
            serde_json::from_str(&without_actions.to_json_string()).unwrap();
        assert!(
            v2.get("available_actions").is_none(),
            "available_actions omitted when empty: {v2}"
        );
    }

    #[test]
    fn with_repair_hint_overrides_default() {
        let err = ToolError::permission("auditor did not pass")
            .with_code("auditor_pass_missing")
            .with_repair_hint("Obtain an auditor pass record, then retry.");
        let v: serde_json::Value = serde_json::from_str(&err.to_json_string()).unwrap();
        assert_eq!(v["error"], "auditor_pass_missing");
        assert_eq!(v["repair_hint"], "Obtain an auditor pass record, then retry.");
    }
}
