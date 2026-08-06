//! Channel-neutral task completion semantics.
//!
//! Workflow marks tasks `Succeeded` when the child session ends cleanly, even when
//! promotion-gate agents report `"status": "fail"` in JSON. Types here normalize
//! that distinction so gateways, reports, and channel adapters (TUI, WhatsApp, …)
//! share one interpretation without embedding transport-specific formatting.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalized gate / verdict outcome from an agent's final reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOutcome {
    Pass,
    Fail,
    Partial,
    UnableToEvaluate,
    ClarificationNeeded,
}

impl AgentOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Partial => "partial",
            Self::UnableToEvaluate => "unable_to_evaluate",
            Self::ClarificationNeeded => "clarification_needed",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        parse_status_str(s)
    }

    /// Whether this outcome satisfies a promotion gate (install may proceed).
    pub fn promotion_satisfied(self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// How to present a finished workflow task (no emojis — adapters map `severity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSeverity {
    /// Task succeeded and gate passed or was not reported.
    Success,
    /// Task succeeded but gate verdict was negative or inconclusive.
    Caveat,
    /// Workflow task failed (spawn error, crash, etc.).
    Failure,
}

/// Channel-neutral presentation hints for a completed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompletionPresentation {
    pub task_succeeded: bool,
    pub outcome: Option<AgentOutcome>,
    pub severity: CompletionSeverity,
}

impl TaskCompletionPresentation {
    pub fn from_workflow_succeeded(agent_outcome: Option<AgentOutcome>) -> Self {
        let severity = match agent_outcome {
            Some(o) if o.promotion_satisfied() => CompletionSeverity::Success,
            Some(_) => CompletionSeverity::Caveat,
            None => CompletionSeverity::Success,
        };
        Self {
            task_succeeded: true,
            outcome: agent_outcome,
            severity,
        }
    }

    pub fn from_workflow_failed() -> Self {
        Self {
            task_succeeded: false,
            outcome: None,
            severity: CompletionSeverity::Failure,
        }
    }

    pub fn from_event_payload(payload: &Value, task_succeeded: bool) -> Self {
        let outcome = payload
            .get("agent_outcome")
            .and_then(|v| v.as_str())
            .and_then(AgentOutcome::parse_str)
            .or_else(|| {
                payload
                    .get("result_summary")
                    .and_then(|v| v.as_str())
                    .and_then(extract_agent_outcome)
            });
        if task_succeeded {
            Self::from_workflow_succeeded(outcome)
        } else {
            Self::from_workflow_failed()
        }
    }

    pub fn gate_caveat(&self) -> bool {
        self.task_succeeded && self.severity == CompletionSeverity::Caveat
    }

    /// Lifecycle / status label (plain text, no emoji).
    pub fn lifecycle_stage(&self) -> String {
        if !self.task_succeeded {
            return "failed".to_string();
        }
        match self.outcome {
            None | Some(AgentOutcome::Pass) => "completed".to_string(),
            Some(AgentOutcome::Fail) => "completed (gate: fail)".to_string(),
            Some(AgentOutcome::Partial) => "completed (gate: partial)".to_string(),
            Some(AgentOutcome::UnableToEvaluate) => "completed (gate: skipped)".to_string(),
            Some(AgentOutcome::ClarificationNeeded) => "completed (gate: needs input)".to_string(),
        }
    }

    /// Short status for tables (session overview, ledgers).
    pub fn status_label(&self) -> String {
        if !self.task_succeeded {
            return "failed".to_string();
        }
        self.lifecycle_stage()
    }

    pub fn severity_class(&self) -> &'static str {
        match self.severity {
            CompletionSeverity::Success => "success",
            CompletionSeverity::Caveat => "caveat",
            CompletionSeverity::Failure => "failure",
        }
    }

    /// Optional suffix for expanded one-line messages (` — gate: fail`).
    pub fn detail_suffix(&self) -> Option<&'static str> {
        if !self.gate_caveat() {
            return None;
        }
        Some(match self.outcome? {
            AgentOutcome::Fail => " — gate: fail",
            AgentOutcome::Partial => " — gate: partial",
            AgentOutcome::UnableToEvaluate => " — gate: skipped",
            AgentOutcome::ClarificationNeeded => " — gate: needs input",
            AgentOutcome::Pass => return None,
        })
    }
}

/// Extract a gate/verdict outcome from an agent's final reply text, if present.
pub fn extract_agent_outcome(reply: &str) -> Option<AgentOutcome> {
    let json = extract_json_value(reply)?;
    outcome_from_value(&json)
}

/// Attach `agent_outcome` and `gate_satisfied` to a workflow tool task entry.
pub fn enrich_task_status_entry(entry: &mut Value, result_summary: Option<&str>) {
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let outcome = obj
        .get("agent_outcome")
        .and_then(|v| v.as_str())
        .and_then(AgentOutcome::parse_str)
        .or_else(|| result_summary.and_then(extract_agent_outcome));
    if let Some(o) = outcome {
        obj.insert(
            "agent_outcome".to_string(),
            Value::String(o.as_str().to_string()),
        );
        obj.insert(
            "gate_satisfied".to_string(),
            Value::Bool(o.promotion_satisfied()),
        );
    }
}

/// True if any succeeded task in the list has an unsatisfied gate verdict.
pub fn any_gate_unsatisfied(tasks: &[Value]) -> bool {
    tasks.iter().any(|t| {
        t.get("gate_satisfied")
            .and_then(|v| v.as_bool())
            == Some(false)
    })
}

/// True if any task entry's `result_summary` is a shortened copy, i.e. its full
/// reply lives behind `full_result_ref`.
pub fn any_result_truncated(tasks: &[Value]) -> bool {
    tasks.iter().any(|t| {
        t.get("result_summary_truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    })
}

/// Neutral join message for `workflow.wait` (no transport formatting).
///
/// `any_truncated` matters because "you may proceed with the results" is a
/// licence to act on `result_summary`. When a reply was too large to inline,
/// that field is a shortened copy and acting on it means acting on partial data —
/// so the message has to say where the whole thing is instead.
pub fn workflow_wait_join_message(
    all_done: bool,
    any_failed: bool,
    any_not_found: bool,
    any_gate_fail: bool,
    any_truncated: bool,
    waited_secs: u64,
) -> String {
    if all_done {
        if any_failed {
            if waited_secs == 0 {
                "All tasks completed (some failed). Review task results and proceed.".to_string()
            } else {
                format!(
                    "All tasks completed after {}s (some failed). Review task results and proceed.",
                    waited_secs
                )
            }
        } else if any_gate_fail {
            if waited_secs == 0 {
                "All tasks finished; some gate verdicts did not pass. Review agent_outcome and result_summary on each task.".to_string()
            } else {
                format!(
                    "All tasks finished after {}s; some gate verdicts did not pass. Review agent_outcome on each task.",
                    waited_secs
                )
            }
        } else if any_truncated {
            let suffix = "Some replies were too large to inline: those tasks carry \
                          result_summary_truncated=true, and their full payload is at \
                          full_result_ref — read it with content.read rather than acting on \
                          result_summary or repeating the work.";
            if waited_secs == 0 {
                format!("All tasks completed successfully. {suffix}")
            } else {
                format!("All tasks completed successfully after {waited_secs}s. {suffix}")
            }
        } else if waited_secs == 0 {
            "All tasks completed successfully. You may proceed with the results.".to_string()
        } else {
            format!(
                "All tasks completed successfully after {}s. You may proceed with the results.",
                waited_secs
            )
        }
    } else if any_not_found {
        "One or more tasks were not found. Verify task_ids and workflow_id.".to_string()
    } else if waited_secs == 0 {
        "Some tasks are still running. Call workflow.wait with timeout_secs > 0 to block until they finish, or continue with other work.".to_string()
    } else {
        format!(
            "Timed out after {}s. Some tasks are still running. Call workflow.wait again or proceed with partial results.",
            waited_secs
        )
    }
}

fn outcome_from_value(v: &Value) -> Option<AgentOutcome> {
    if let Some(status) = v.get("status").and_then(|s| s.as_str()) {
        return parse_status_str(status);
    }
    for key in [
        "evaluator_pass",
        "auditor_pass",
        "static_evaluator_pass",
        "unit_test_runner_pass",
        "sealed_evaluator_pass",
    ] {
        if let Some(pass) = v.get(key).and_then(|b| b.as_bool()) {
            return Some(if pass {
                AgentOutcome::Pass
            } else {
                AgentOutcome::Fail
            });
        }
    }
    None
}

fn parse_status_str(s: &str) -> Option<AgentOutcome> {
    match s.trim().to_ascii_lowercase().as_str() {
        "pass" | "passed" | "ok" | "success" | "succeeded" => Some(AgentOutcome::Pass),
        "fail" | "failed" | "error" => Some(AgentOutcome::Fail),
        "partial" => Some(AgentOutcome::Partial),
        "unable_to_evaluate" | "unable" | "skip" | "skipped" => Some(AgentOutcome::UnableToEvaluate),
        "clarification_needed" | "needs_clarification" => Some(AgentOutcome::ClarificationNeeded),
        _ => None,
    }
}

fn extract_json_value(reply: &str) -> Option<Value> {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    if let Some(inner) = extract_first_fenced_json(trimmed) {
        if let Ok(v) = serde_json::from_str(&inner) {
            return Some(v);
        }
    }
    let start = trimmed.find('{')?;
    let mut depth = 0i32;
    for (i, ch) in trimmed[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &trimmed[start..start + i + 1];
                    if let Ok(v) = serde_json::from_str(slice) {
                        return Some(v);
                    }
                    break;
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_first_fenced_json(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    while pos < len {
        if bytes[pos] == b'`' && pos + 2 < len && &bytes[pos..pos + 3] == b"```" {
            pos += 3;
            while pos < len && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\n' {
                pos += 1;
            }
            let content_start = pos;
            let mut search = content_start;
            while search < len {
                if bytes[search] == b'`'
                    && search + 2 < len
                    && &bytes[search..search + 3] == b"```"
                {
                    let content = s[content_start..search].trim();
                    if serde_json::from_str::<Value>(content).is_ok() {
                        return Some(content.to_owned());
                    }
                    break;
                }
                search += 1;
            }
            pos = if search + 3 < len { search + 3 } else { len };
        } else {
            pos += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_fail_json() {
        let reply = r#"{"status":"fail","evaluator_pass":false,"summary":"No test files found"}"#;
        assert_eq!(extract_agent_outcome(reply), Some(AgentOutcome::Fail));
    }

    #[test]
    fn presentation_lifecycle_gate_fail() {
        let p = TaskCompletionPresentation::from_workflow_succeeded(Some(AgentOutcome::Fail));
        assert_eq!(p.lifecycle_stage(), "completed (gate: fail)");
        assert!(p.gate_caveat());
        assert_eq!(p.severity_class(), "caveat");
    }

    #[test]
    fn workflow_wait_message_gate_fail() {
        let msg = workflow_wait_join_message(true, false, false, true, false, 30);
        assert!(msg.contains("gate verdicts"));
    }

    /// "You may proceed with the results" is a licence to act on
    /// `result_summary`. When a reply was too large to inline, that field holds a
    /// shortened copy, so the message must point at the full payload instead —
    /// otherwise the parent acts on partial data or redoes the work.
    #[test]
    fn workflow_wait_message_points_at_the_full_payload_when_truncated() {
        let msg = workflow_wait_join_message(true, false, false, false, true, 0);
        assert!(msg.contains("full_result_ref"), "got {msg}");
        assert!(
            !msg.contains("may proceed with the results"),
            "must not license acting on a shortened summary: {msg}"
        );

        let untruncated = workflow_wait_join_message(true, false, false, false, false, 0);
        assert!(untruncated.contains("may proceed with the results"));
    }

    #[test]
    fn any_result_truncated_reads_the_task_flag() {
        let clean = vec![serde_json::json!({"task_id": "t1"})];
        assert!(!any_result_truncated(&clean));

        let flagged = vec![
            serde_json::json!({"task_id": "t1"}),
            serde_json::json!({"task_id": "t2", "result_summary_truncated": true}),
        ];
        assert!(any_result_truncated(&flagged));
    }
}
