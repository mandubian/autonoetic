use autonoetic_types::tool_error::{
    FailureClass, RetryAdvice, SideEffectState, ToolError, ToolErrorType,
};
use autonoetic_types::workflow::TaskRunStatus;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowFailureMetadata {
    pub failure_class: Option<FailureClass>,
    pub retry_advice: Option<RetryAdvice>,
    pub retryable: Option<bool>,
    pub requires_external_event: Option<bool>,
    pub requires_human: Option<bool>,
    pub side_effect_state: Option<SideEffectState>,
}

impl WorkflowFailureMetadata {
    fn approval_pending() -> Self {
        Self {
            failure_class: Some(FailureClass::ApprovalPending),
            retry_advice: Some(RetryAdvice::Wait),
            retryable: Some(false),
            requires_external_event: Some(true),
            requires_human: Some(true),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn policy_denied() -> Self {
        Self {
            failure_class: Some(FailureClass::PolicyDenied),
            retry_advice: Some(RetryAdvice::DoNotRetry),
            retryable: Some(false),
            requires_external_event: Some(false),
            requires_human: Some(true),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn child_cancelled() -> Self {
        Self {
            failure_class: Some(FailureClass::ChildCancelled),
            retry_advice: Some(RetryAdvice::DoNotRetry),
            retryable: Some(false),
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::Unknown),
        }
    }

    fn install_conflict() -> Self {
        Self {
            failure_class: Some(FailureClass::InstallConflict),
            retry_advice: Some(RetryAdvice::DoNotRetry),
            retryable: Some(false),
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn timeout() -> Self {
        Self {
            failure_class: Some(FailureClass::Timeout),
            retry_advice: None,
            retryable: Some(true),
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::Unknown),
        }
    }

    fn timeout_waiting_on_human() -> Self {
        Self {
            failure_class: Some(FailureClass::Timeout),
            retry_advice: Some(RetryAdvice::Wait),
            retryable: Some(false),
            requires_external_event: Some(true),
            requires_human: Some(true),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn transient_infra() -> Self {
        Self {
            failure_class: Some(FailureClass::TransientInfra),
            retry_advice: None,
            retryable: Some(true),
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn schema_validation_failed() -> Self {
        Self {
            failure_class: Some(FailureClass::SchemaValidationFailed),
            retry_advice: Some(RetryAdvice::FixArtifactThenRetry),
            retryable: Some(true),
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    /// The host is missing the sandbox driver the agent requires (e.g. `bwrap`
    /// not on PATH). A node-level infrastructure gap, not a transient blip:
    /// retrying the same exec can never succeed, so mark it non-retryable and
    /// surface it as "unable to evaluate" rather than a hard failure. (#600)
    fn sandbox_unavailable() -> Self {
        Self {
            failure_class: Some(FailureClass::GateUnableToEvaluate),
            retry_advice: Some(RetryAdvice::DoNotRetry),
            retryable: Some(false),
            requires_external_event: Some(false),
            requires_human: Some(true),
            side_effect_state: Some(SideEffectState::NoSideEffect),
        }
    }

    fn unknown_failure() -> Self {
        Self {
            failure_class: Some(FailureClass::Unknown),
            retry_advice: None,
            retryable: None,
            requires_external_event: Some(false),
            requires_human: Some(false),
            side_effect_state: Some(SideEffectState::Unknown),
        }
    }

    pub(crate) fn apply_to_tool_error(&self, tool_error: &mut ToolError) {
        if tool_error.failure_class.is_none() {
            tool_error.failure_class = self.failure_class;
        }
        if tool_error.retry_advice.is_none() {
            tool_error.retry_advice = self.retry_advice;
        }
        if tool_error.retryable.is_none() {
            tool_error.retryable = self.retryable;
        }
        if tool_error.requires_external_event.is_none() {
            tool_error.requires_external_event = self.requires_external_event;
        }
        if tool_error.requires_human.is_none() {
            tool_error.requires_human = self.requires_human;
        }
        if tool_error.side_effect_state.is_none() {
            tool_error.side_effect_state = self.side_effect_state;
        }
    }

    pub(crate) fn apply_to_json_map(&self, map: &mut Map<String, Value>) {
        insert_if_missing(
            map,
            "failure_class",
            self.failure_class.and_then(enum_to_value),
        );
        insert_if_missing(
            map,
            "retry_advice",
            self.retry_advice.and_then(enum_to_value),
        );
        insert_if_missing(map, "retryable", self.retryable.map(Value::Bool));
        insert_if_missing(
            map,
            "requires_external_event",
            self.requires_external_event.map(Value::Bool),
        );
        insert_if_missing(
            map,
            "requires_human",
            self.requires_human.map(Value::Bool),
        );
        insert_if_missing(
            map,
            "side_effect_state",
            self.side_effect_state.and_then(enum_to_value),
        );
    }
}

fn enum_to_value<T: serde::Serialize>(value: T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

fn insert_if_missing(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if !map.contains_key(key) {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    }
}

/// P-5.14: classification is a pure function of gateway-observed state, and the
/// `ToolErrorType` is the strongest such signal. For the variants that map to a
/// failure class UNAMBIGUOUSLY, decide from the type alone — never let agent
/// prose override it. Variants whose meaning genuinely depends on context
/// (`Validation` → schema vs. contract, `Conflict` → install-conflict only in
/// the revision context, etc.) return `None`, routing them to the logged prose
/// fallback below, which preserves today's behavior for them.
fn classify_by_type(error_type: &ToolErrorType) -> Option<WorkflowFailureMetadata> {
    match error_type {
        ToolErrorType::SandboxUnavailable => Some(WorkflowFailureMetadata::sandbox_unavailable()),
        ToolErrorType::Permission => Some(WorkflowFailureMetadata::policy_denied()),
        ToolErrorType::Timeout => Some(WorkflowFailureMetadata::timeout()),
        // `Resource` is NOT unambiguous: a missing sandbox driver rides on a
        // `Resource` error carrying the `[sandbox_driver_unavailable]` marker
        // (the pre-typed representation), which must classify as
        // `GateUnableToEvaluate` (do-not-retry), NOT `transient_infra`
        // (retryable). Defer `Resource` to the prose fallback, which checks
        // that marker before defaulting a plain resource error to transient.
        ToolErrorType::Resource
        | ToolErrorType::Validation
        | ToolErrorType::Conflict
        | ToolErrorType::QuotaExceeded
        | ToolErrorType::NotFound
        | ToolErrorType::Execution
        | ToolErrorType::Fatal => None,
    }
}

fn classify_message(message: &str, error_type: ToolErrorType) -> WorkflowFailureMetadata {
    // P-5.14: typed classification first — the type is gateway-observed state
    // and the authoritative signal. Prose is consulted only when the type is
    // insufficient (`classify_by_type` returned `None`).
    if let Some(metadata) = classify_by_type(&error_type) {
        return metadata;
    }

    // Prose fallback (P-5.14 last-resort). Reached for error types without an
    // unambiguous type-only mapping. Logged when reached, so reliance on prose
    // (a potential discretion leak) is observable in traces.
    tracing::debug!(
        target: "failure_classification",
        error_type = ?error_type,
        "prose fallback used for failure classification (P-5.14 last-resort)"
    );

    let lower = message.to_ascii_lowercase();

    if matches!(error_type, ToolErrorType::SandboxUnavailable)
        || lower.contains("sandbox_driver_unavailable")
        || (lower.contains("sandbox driver") && lower.contains("not found on path"))
    {
        return WorkflowFailureMetadata::sandbox_unavailable();
    }
    if lower.contains("approval required") || lower.contains("approval_pending") {
        return WorkflowFailureMetadata::approval_pending();
    }
    if lower.contains("install conflict")
        || lower.contains("active revision exists")
        || lower.contains("archived revision exists")
    {
        return WorkflowFailureMetadata::install_conflict();
    }
    if lower.contains("approval_rejected") || lower.contains("denied by policy") {
        return WorkflowFailureMetadata::policy_denied();
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return WorkflowFailureMetadata::timeout();
    }
    if lower.contains("connection refused")
        || lower.contains("transport reset")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("500")
        || lower.contains("529")
        || lower.contains("api error 5")
        || lower.contains("overloaded")
        || lower.contains("peg-native")
        || lower.contains("internal server error")
        || lower.contains("temporarily unavailable")
    {
        return WorkflowFailureMetadata::transient_infra();
    }
    if matches!(error_type, ToolErrorType::Validation)
        && (lower.contains("invalid json")
            || lower.contains("schema")
            || lower.contains("missing field")
            || lower.contains("must not be empty"))
    {
        return WorkflowFailureMetadata::schema_validation_failed();
    }
    if matches!(error_type, ToolErrorType::Permission) {
        return WorkflowFailureMetadata::policy_denied();
    }
    if matches!(error_type, ToolErrorType::Timeout) {
        return WorkflowFailureMetadata::timeout();
    }
    if matches!(error_type, ToolErrorType::Resource) {
        return WorkflowFailureMetadata::transient_infra();
    }
    WorkflowFailureMetadata::unknown_failure()
}

pub(crate) fn decorate_tool_error(mut tool_error: ToolError) -> ToolError {
    let metadata = classify_message(&tool_error.message, tool_error.error_type.clone());
    metadata.apply_to_tool_error(&mut tool_error);
    tool_error
}

pub(crate) fn normalize_tool_result_json(result_json: &str) -> String {
    let Ok(mut parsed) = serde_json::from_str::<Value>(result_json) else {
        return result_json.to_string();
    };

    let Some(object) = parsed.as_object_mut() else {
        return result_json.to_string();
    };

    let metadata = if object
        .get("approval_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(WorkflowFailureMetadata::approval_pending())
    } else if object
        .get("approval_rejected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(WorkflowFailureMetadata::policy_denied())
    } else if object.get("ok").and_then(Value::as_bool) == Some(false) {
        if let Some(error_type) = object.get("error_type").and_then(Value::as_str) {
            let error_type = match error_type {
                "validation" => ToolErrorType::Validation,
                "permission" => ToolErrorType::Permission,
                "resource" => ToolErrorType::Resource,
                "execution" => ToolErrorType::Execution,
                "fatal" => ToolErrorType::Fatal,
                "conflict" => ToolErrorType::Conflict,
                "quota_exceeded" => ToolErrorType::QuotaExceeded,
                "not_found" => ToolErrorType::NotFound,
                "timeout" => ToolErrorType::Timeout,
                "sandbox_unavailable" => ToolErrorType::SandboxUnavailable,
                _ => ToolErrorType::Execution,
            };
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(classify_message(message, error_type))
        } else {
            None
        }
    } else {
        None
    };

    if let Some(metadata) = metadata {
        metadata.apply_to_json_map(object);
        serde_json::to_string(&parsed).unwrap_or_else(|_| result_json.to_string())
    } else {
        result_json.to_string()
    }
}

pub(crate) fn classify_task_status(
    status: TaskRunStatus,
    result_summary: Option<&str>,
) -> Option<WorkflowFailureMetadata> {
    match status {
        TaskRunStatus::AwaitingApproval => Some(WorkflowFailureMetadata::approval_pending()),
        TaskRunStatus::Cancelled => Some(WorkflowFailureMetadata::child_cancelled()),
        TaskRunStatus::Aborted => Some(WorkflowFailureMetadata::unknown_failure()),
        TaskRunStatus::Failed => {
            let summary = result_summary.unwrap_or_default().to_ascii_lowercase();
            if summary.starts_with("approval_rejected") {
                Some(WorkflowFailureMetadata::policy_denied())
            } else if summary.contains("approval timed out") {
                Some(WorkflowFailureMetadata::timeout_waiting_on_human())
            } else if summary.contains("install conflict")
                || summary.contains("active revision exists")
                || summary.contains("archived revision exists")
            {
                Some(WorkflowFailureMetadata::install_conflict())
            } else if summary.contains("connection refused")
                || summary.contains("transport reset")
                || summary.contains("502")
                || summary.contains("503")
                || summary.contains("504")
                || summary.contains("500")
                || summary.contains("529")
                || summary.contains("api error 5")
                || summary.contains("overloaded")
                || summary.contains("peg-native")
                || summary.contains("internal server error")
                || summary.contains("temporarily unavailable")
            {
                Some(WorkflowFailureMetadata::transient_infra())
            } else if summary.contains("timed out") || summary.contains("timeout") {
                Some(WorkflowFailureMetadata::timeout())
            } else if summary.contains("validation failed")
                || summary.contains("response_validation")
                || summary.contains("schema")
            {
                Some(WorkflowFailureMetadata::schema_validation_failed())
            } else {
                Some(WorkflowFailureMetadata::unknown_failure())
            }
        }
        _ => None,
    }
}

pub(crate) fn metadata_for_failure_class(failure_class: FailureClass) -> WorkflowFailureMetadata {
    match failure_class {
        FailureClass::ApprovalPending => WorkflowFailureMetadata::approval_pending(),
        FailureClass::PolicyDenied => WorkflowFailureMetadata::policy_denied(),
        FailureClass::ChildCancelled => WorkflowFailureMetadata::child_cancelled(),
        FailureClass::InstallConflict => WorkflowFailureMetadata::install_conflict(),
        FailureClass::Timeout => WorkflowFailureMetadata::timeout(),
        FailureClass::TransientInfra => WorkflowFailureMetadata::transient_infra(),
        FailureClass::SchemaValidationFailed => WorkflowFailureMetadata::schema_validation_failed(),
        FailureClass::AwaitingUserInput
        | FailureClass::ArtifactInvalid
        | FailureClass::DependencyMissing
        | FailureClass::GateUnsatisfied
        | FailureClass::GateUnableToEvaluate
        | FailureClass::TaskContractInvalid
        | FailureClass::Unknown => WorkflowFailureMetadata::unknown_failure(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_required_result_is_decorated() {
        let result = normalize_tool_result_json(
            &serde_json::json!({
                "ok": false,
                "approval_required": true,
                "request_id": "apr-123"
            })
            .to_string(),
        );
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["failure_class"], "approval_pending");
        assert_eq!(parsed["retry_advice"], "wait");
        assert_eq!(parsed["requires_external_event"], true);
        assert_eq!(parsed["requires_human"], true);
        assert_eq!(parsed["side_effect_state"], "none");
    }

    #[test]
    fn timeout_tool_error_is_decorated() {
        let err = decorate_tool_error(ToolError::timeout("request timed out", None::<String>));
        assert_eq!(err.failure_class, Some(FailureClass::Timeout));
        assert_eq!(err.retryable, Some(true));
        assert_eq!(err.side_effect_state, Some(SideEffectState::Unknown));
    }

    #[test]
    fn approval_timeout_task_status_is_classified_as_timeout() {
        let metadata = classify_task_status(TaskRunStatus::Failed, Some("Approval timed out"))
            .expect("timeout metadata");
        assert_eq!(metadata.failure_class, Some(FailureClass::Timeout));
        assert_eq!(metadata.retry_advice, Some(RetryAdvice::Wait));
        assert_eq!(metadata.requires_external_event, Some(true));
        assert_eq!(metadata.requires_human, Some(true));
    }

    #[test]
    fn cancelled_task_status_is_classified_as_child_cancelled() {
        let metadata = classify_task_status(TaskRunStatus::Cancelled, None).expect("cancelled metadata");
        assert_eq!(metadata.failure_class, Some(FailureClass::ChildCancelled));
        assert_eq!(metadata.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(metadata.retryable, Some(false));
    }

    #[test]
    fn install_conflict_tool_error_is_decorated() {
        let err = decorate_tool_error(ToolError::conflict(
            "active revision exists for this agent",
            None::<String>,
        ));
        assert_eq!(err.failure_class, Some(FailureClass::InstallConflict));
        assert_eq!(err.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(err.retryable, Some(false));
    }

    // A missing sandbox driver must be terminal & unable-to-evaluate, not the
    // generic retryable Resource→transient_infra classification. (#600)
    #[test]
    fn sandbox_driver_unavailable_is_terminal_unable_to_evaluate() {
        // The marker the SandboxRunner stamps on a spawn ENOENT.
        let msg = "sandbox driver 'bwrap' not found on PATH — ... [sandbox_driver_unavailable]";
        let err = decorate_tool_error(ToolError::resource(msg, None::<String>));
        assert_eq!(err.failure_class, Some(FailureClass::GateUnableToEvaluate));
        assert_eq!(err.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(err.retryable, Some(false));
    }

    #[test]
    fn typed_sandbox_unavailable_is_terminal_unable_to_evaluate() {
        let err = decorate_tool_error(ToolError::sandbox_unavailable(
            "sandbox driver 'bwrap' not found on PATH",
        ));
        assert_eq!(err.error_type, ToolErrorType::SandboxUnavailable);
        assert_eq!(err.failure_class, Some(FailureClass::GateUnableToEvaluate));
        assert_eq!(err.retryable, Some(false));
    }

    #[test]
    fn plain_resource_error_stays_retryable() {
        let err = decorate_tool_error(ToolError::resource("rate limited", None::<String>));
        assert_eq!(err.failure_class, Some(FailureClass::TransientInfra));
        assert_eq!(err.retryable, Some(true));
    }

    // P-5.14: classification is deterministic from an empty message —
    // Permission/Timeout/SandboxUnavailable via the fast typed mapping, and
    // Resource via the fallback's type branch (Resource is deliberately NOT in
    // the fast map — see classify_by_type — yet still resolves with no prose).
    #[test]
    fn typed_classification_needs_no_message() {
        assert_eq!(
            classify_message("", ToolErrorType::Permission).failure_class,
            Some(FailureClass::PolicyDenied)
        );
        assert_eq!(
            classify_message("", ToolErrorType::Timeout).failure_class,
            Some(FailureClass::Timeout)
        );
        assert_eq!(
            classify_message("", ToolErrorType::Resource).failure_class,
            Some(FailureClass::TransientInfra)
        );
        assert_eq!(
            classify_message("", ToolErrorType::SandboxUnavailable).failure_class,
            Some(FailureClass::GateUnableToEvaluate)
        );
    }

    // Classification tests for LLM/server-style transient infrastructure
    // failures. These exercise the prose fallback string heuristics that map
    // HTTP 5xx and related overload phrases to TransientInfra.
    #[test]
    fn llm_5xx_task_status_is_transient_infra() {
        let cases = [
            "OpenAI API error 500: internal server error",
            "Anthropic API error 529",
            "model produced output that does not match the expected peg-native format",
            "upstream returned api error 500",
            "connection refused",
            "transport reset",
            "bad gateway 502",
            "service unavailable 503",
            "gateway timeout 504",
            "server is temporarily unavailable",
        ];
        for summary in cases {
            let metadata = classify_task_status(TaskRunStatus::Failed, Some(summary)).expect(summary);
            assert_eq!(
                metadata.failure_class,
                Some(FailureClass::TransientInfra),
                "{summary} should be transient_infra"
            );
            assert_eq!(metadata.retryable, Some(true));
            assert_eq!(metadata.side_effect_state, Some(SideEffectState::NoSideEffect));
        }
    }

    #[test]
    fn llm_5xx_message_is_transient_infra() {
        let cases = [
            "OpenAI API error 500: internal server error",
            "Anthropic API error 529",
            "model produced output that does not match the expected peg-native format",
            "upstream returned api error 5xx",
        ];
        for msg in cases {
            let err = decorate_tool_error(ToolError::execution(msg, None::<String>));
            assert_eq!(
                err.failure_class,
                Some(FailureClass::TransientInfra),
                "{msg} should be transient_infra"
            );
            assert_eq!(err.retryable, Some(true));
        }
    }

    #[test]
    fn explicit_500_in_message_is_transient_infra() {
        let err = decorate_tool_error(ToolError::execution("500", None::<String>));
        assert_eq!(err.failure_class, Some(FailureClass::TransientInfra));
    }

    #[test]
    fn non_retryable_validation_error_is_not_transient_infra() {
        let err = decorate_tool_error(ToolError::validation(
            "invalid json missing field",
            None::<String>,
        ));
        assert_eq!(err.failure_class, Some(FailureClass::SchemaValidationFailed));
    }
}
