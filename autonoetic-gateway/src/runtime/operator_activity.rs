//! Shared operator-activity classification (session report + live feed).

use autonoetic_types::operator_activity::{
    OperatorActivityKind, OperatorActivityRecord, OperatorActivityRefs, OperatorActivitySeverity,
};
use serde_json::Value;

const SUMMARY_MAX_CHARS: usize = 240;

/// Tools whose successful completion is suppressed from the operator feed (poll / noise).
const SUCCESS_DENYLIST: &[&str] = &[
    "execution_search",
    "knowledge_search",
    "approval_list",
    "digest_query",
    "quality_trend_report",
    "session_search",
    "observability_search",
];

#[derive(Debug, Clone)]
pub struct OperatorActivityDraft {
    pub kind: OperatorActivityKind,
    pub severity: OperatorActivitySeverity,
    pub summary: String,
    pub refs: OperatorActivityRefs,
}

impl OperatorActivityDraft {
    pub fn into_record(
        self,
        root_session_id: String,
        session_id: String,
        agent_id: String,
        workflow_id: Option<String>,
        task_id: Option<String>,
        turn_id: Option<String>,
        tool_name: Option<String>,
        causal_event_id: Option<String>,
        workflow_event_id: Option<String>,
    ) -> OperatorActivityRecord {
        OperatorActivityRecord {
            activity_id: format!("oa-{}", uuid::Uuid::new_v4()),
            root_session_id,
            session_id,
            agent_id,
            workflow_id,
            task_id,
            turn_id,
            occurred_at: chrono::Utc::now().to_rfc3339(),
            kind: self.kind,
            severity: self.severity,
            summary: self.summary,
            tool_name,
            causal_event_id,
            workflow_event_id,
            refs: self.refs,
        }
    }
}

/// Whether a completed tool call should appear in the operator activity feed.
pub fn classify_tool_activity(
    tool_name: &str,
    _arguments_json: &str,
    result_json: &str,
) -> Option<OperatorActivityDraft> {
    let parsed = serde_json::from_str::<Value>(result_json).ok();

    let is_approval = parsed
        .as_ref()
        .and_then(|v| v.get("approval_required").and_then(|x| x.as_bool()))
        == Some(true);

    let mut ok = parsed
        .as_ref()
        .and_then(|v| v.get("ok").and_then(|x| x.as_bool()))
        != Some(false);

    if tool_name == "workflow_wait" {
        if parsed
            .as_ref()
            .and_then(|v| v.get("any_failed").and_then(|x| x.as_bool()))
            == Some(true)
        {
            ok = false;
        }
    }

    if is_approval {
        let mut refs = OperatorActivityRefs::default();
        refs.approval_request_id = parsed
            .as_ref()
            .and_then(|v| v.get("request_id"))
            .and_then(|x| x.as_str())
            .map(str::to_string);
        return Some(OperatorActivityDraft {
            kind: OperatorActivityKind::ApprovalRequired,
            severity: OperatorActivitySeverity::Attention,
            summary: summarize_approval(parsed.as_ref()),
            refs,
        });
    }

    if !ok {
        if is_poll_tool(tool_name) && !poll_result_is_important(tool_name, parsed.as_ref()) {
            return None;
        }
        return Some(OperatorActivityDraft {
            kind: OperatorActivityKind::ToolFailed,
            severity: OperatorActivitySeverity::Error,
            summary: summarize_tool_error(tool_name, parsed.as_ref(), result_json),
            refs: refs_from_result(parsed.as_ref()),
        });
    }

    if is_poll_tool(tool_name) {
        if !poll_result_is_important(tool_name, parsed.as_ref()) {
            return None;
        }
        let kind = if tool_name == "workflow_wait" {
            OperatorActivityKind::WorkflowJoin
        } else {
            OperatorActivityKind::ToolCompleted
        };
        return Some(OperatorActivityDraft {
            kind,
            severity: if tool_name == "workflow_wait"
                && parsed
                    .as_ref()
                    .and_then(|v| v.get("any_failed").and_then(|x| x.as_bool()))
                    == Some(true)
            {
                OperatorActivitySeverity::Error
            } else {
                OperatorActivitySeverity::Progress
            },
            summary: summarize_tool_result(tool_name, parsed.as_ref(), result_json),
            refs: refs_from_result(parsed.as_ref()),
        });
    }

    if SUCCESS_DENYLIST.contains(&tool_name) {
        return None;
    }

    let kind = match tool_name {
        "agent_spawn" => OperatorActivityKind::Delegation,
        _ => OperatorActivityKind::ToolCompleted,
    };

    Some(OperatorActivityDraft {
        kind,
        severity: OperatorActivitySeverity::Progress,
        summary: summarize_tool_result(tool_name, parsed.as_ref(), result_json),
        refs: refs_from_result(parsed.as_ref()),
    })
}

pub fn classify_session_lifecycle(
    outcome: autonoetic_types::session_outcome::SessionCloseOutcome,
    tool_steps_in_ingest: u32,
) -> Option<OperatorActivityDraft> {
    if !outcome.is_completed_empty() || !outcome.is_jsonrpc_spawn() || tool_steps_in_ingest == 0 {
        return None;
    }
    Some(OperatorActivityDraft {
        kind: OperatorActivityKind::SessionLifecycle,
        severity: OperatorActivitySeverity::Attention,
        summary: format!(
            "session ended with no assistant message after {} tool step(s)",
            tool_steps_in_ingest
        ),
        refs: OperatorActivityRefs::default(),
    })
}

pub fn classify_workflow_event(event_type: &str) -> Option<OperatorActivityDraft> {
    match event_type {
        "planframe.proposed" => Some(OperatorActivityDraft {
            kind: OperatorActivityKind::PlanProposal,
            severity: OperatorActivitySeverity::Attention,
            summary: "plan awaiting operator approval".to_string(),
            refs: OperatorActivityRefs::default(),
        }),
        "task.failed" => Some(OperatorActivityDraft {
            kind: OperatorActivityKind::ToolFailed,
            severity: OperatorActivitySeverity::Error,
            summary: "workflow task failed".to_string(),
            refs: OperatorActivityRefs::default(),
        }),
        _ => None,
    }
}

/// Build a passive operator-activity advisory for a Sentinel divergence verdict.
/// This replaces the pushed DivergenceSentinel UserInteraction popup (Phase 2 D.7a).
pub fn classify_sentinel_notice(
    level: &str,
    agent_id: &str,
    turn: u64,
    signals: &[crate::runtime::trajectory_health::DivergenceSignal],
) -> OperatorActivityDraft {
    use crate::runtime::trajectory_health::{DivergenceSignalKind, SignalSeverity};
    let severity = if level == "critical"
        || signals.iter().any(|s| s.severity == SignalSeverity::Critical)
    {
        OperatorActivitySeverity::Error
    } else {
        OperatorActivitySeverity::Attention
    };

    let signal_summary = if signals.is_empty() {
        "no detailed signals".to_string()
    } else {
        let parts: Vec<String> = signals
            .iter()
            .map(|s| {
                let base = format!("{}={:.2}", s.kind.as_str(), s.current);
                match &s.evidence {
                    Some(e) if !e.is_empty() => format!("{} ({})", base, truncate_chars(e, 80)),
                    _ => base,
                }
            })
            .collect();
        parts.join(", ")
    };

    let summary = format!(
        "Sentinel [{}] for {} at turn {} — {}",
        level, agent_id, turn, signal_summary
    );

    let refs = OperatorActivityRefs::default();
    // Keep repetition-entropy / feedback-incorporated notices advisory-only in kind metadata.
    let has_only_advisory_signals = !signals.is_empty()
        && signals
            .iter()
            .all(|s| s.kind == DivergenceSignalKind::FeedbackIncorporated || s.kind == DivergenceSignalKind::RepetitionEntropy);
    let _ = has_only_advisory_signals; // reserved for future filtering; severity already encodes urgency

    OperatorActivityDraft {
        kind: OperatorActivityKind::SentinelNotice,
        severity,
        summary: truncate_chars(&summary, SUMMARY_MAX_CHARS),
        refs,
    }
}

pub fn is_poll_tool(tool_name: &str) -> bool {
    matches!(tool_name, "workflow_wait" | "workflow_state")
}

pub fn poll_result_is_important(tool_name: &str, parsed: Option<&Value>) -> bool {
    match tool_name {
        "workflow_wait" => {
            let Some(v) = parsed else {
                return false;
            };
            v.get("any_failed").and_then(|x| x.as_bool()) == Some(true)
                || v.get("join_satisfied").and_then(|x| x.as_bool()) == Some(true)
                || v.get("tasks")
                    .and_then(|x| x.as_array())
                    .map(|tasks| {
                        tasks.iter().any(|task| {
                            matches!(
                                task.get("status").and_then(|x| x.as_str()),
                                Some("AwaitingApproval" | "Failed" | "Cancelled" | "Aborted")
                            )
                        })
                    })
                    .unwrap_or(false)
        }
        "workflow_state" => {
            let Some(v) = parsed else {
                return false;
            };
            v.get("failed_task_count")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                > 0
                || v.get("pending_approvals")
                    .and_then(|x| x.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
        }
        _ => false,
    }
}

fn refs_from_result(parsed: Option<&Value>) -> OperatorActivityRefs {
    let mut refs = OperatorActivityRefs::default();
    if let Some(v) = parsed {
        refs.approval_request_id = v
            .get("request_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        refs.plan_id = v
            .get("plan_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        refs.interaction_id = v
            .get("interaction_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        refs.artifact_id = v
            .get("artifact_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
        refs.workbench_id = v
            .get("workbench_id")
            .and_then(|x| x.as_str())
            .map(str::to_string);
    }
    refs
}

fn summarize_approval(parsed: Option<&Value>) -> String {
    let request_id = parsed
        .and_then(|v| v.get("request_id"))
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    let detail = parsed
        .and_then(|v| v.get("summary"))
        .and_then(|x| x.as_str())
        .unwrap_or("pending operator decision");
    truncate_chars(
        &format!("approval `{}` required: {}", request_id, detail),
        SUMMARY_MAX_CHARS,
    )
}

fn summarize_self_describe(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "introspected self".to_string();
    };
    let name = v
        .get("identity")
        .and_then(|i| i.get("name").or_else(|| i.get("agent_id")))
        .and_then(|x| x.as_str())
        .unwrap_or("self");
    let cap_count = v
        .get("may_do")
        .and_then(|m| m.get("capabilities"))
        .and_then(|c| c.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let rights_count = v
        .get("guaranteed")
        .and_then(|g| g.get("rights"))
        .and_then(|r| r.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let tier_count = v
        .get("may_do")
        .and_then(|m| m.get("allowed_tool_tiers"))
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!(
        "introspected {} — {} capabilities, {} rights, {} tool tiers",
        name, cap_count, rights_count, tier_count
    )
}

fn capability_label(cap: &Value) -> String {
    if let Some(s) = cap.as_str() {
        return s.to_string();
    }
    if let Some(obj) = cap.as_object() {
        if obj.len() == 1 {
            if let Some(key) = obj.keys().next() {
                return key.clone();
            }
        }
    }
    cap.to_string()
}

/// Human-readable expansion of a `self_describe` tool result for chat display.
pub fn format_self_describe_for_chat(v: &Value) -> String {
    let agent_id = v
        .get("identity")
        .and_then(|i| i.get("agent_id"))
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    let name = v
        .get("identity")
        .and_then(|i| i.get("name"))
        .and_then(|x| x.as_str())
        .unwrap_or(agent_id);
    let caps: Vec<String> = v
        .get("may_do")
        .and_then(|m| m.get("capabilities"))
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().map(capability_label).collect())
        .unwrap_or_default();
    let tiers: Vec<String> = v
        .get("may_do")
        .and_then(|m| m.get("allowed_tool_tiers"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let rights: Vec<String> = v
        .get("guaranteed")
        .and_then(|g| g.get("rights"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("id").and_then(|x| x.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let paths: Vec<String> = v
        .get("evolution")
        .and_then(|e| e.get("paths"))
        .and_then(|p| p.as_array())
        .map(|arr| arr.iter().filter_map(format_evolution_path).collect())
        .unwrap_or_default();

    let mut out = format!("**{name}** (`{agent_id}`)\n");
    if !caps.is_empty() {
        out.push_str("\n**Capabilities:**\n");
        for cap in &caps {
            out.push_str(&format!("- {cap}\n"));
        }
    }
    if !tiers.is_empty() {
        out.push_str(&format!(
            "\n**Tool tiers:** {} (tools are grouped by tier in the gateway registry)\n",
            tiers.join(", ")
        ));
    }
    if !rights.is_empty() {
        out.push_str(&format!(
            "\n**Rights:** {} constitutional guarantees",
            rights.len()
        ));
        if rights.len() <= 6 {
            out.push_str(&format!(" ({})", rights.join(", ")));
        }
        out.push('\n');
    }
    if !paths.is_empty() {
        out.push_str("\n**Evolution paths:**\n");
        // Not truncated: the list is a bounded const table gateway-side, and an
        // *unavailable* path is exactly what an operator needs to see (#818).
        for path in &paths {
            out.push_str(&format!("- {path}\n"));
        }
    }
    out.trim_end().to_string()
}

/// One evolution path as a chat line.
///
/// Handles both shapes: the derived object emitted by `self_describe`
/// (`{path, available, unavailable_reason, ...}`) and the older plain string,
/// which remote agents on an earlier gateway may still return.
fn format_evolution_path(entry: &Value) -> Option<String> {
    if let Some(s) = entry.as_str() {
        return Some(s.to_string());
    }
    let id = entry.get("path").and_then(|p| p.as_str())?;
    let available = entry
        .get("available")
        .and_then(|a| a.as_bool())
        .unwrap_or(false);
    if available {
        return Some(format!("{id} — available"));
    }
    match entry.get("unavailable_reason").and_then(|r| r.as_str()) {
        Some(reason) => Some(format!("{id} — unavailable ({reason})")),
        None => Some(format!("{id} — unavailable")),
    }
}

pub fn parse_and_format_self_describe_json(raw: &str) -> Option<String> {
    let v = serde_json::from_str::<Value>(raw).ok()?;
    if !looks_like_self_describe_json(&v) {
        return None;
    }
    Some(format_self_describe_for_chat(&v))
}

fn looks_like_self_describe_json(v: &Value) -> bool {
    v.get("evolution").is_some()
        && (v.get("identity").is_some()
            || v.get("may_do").is_some()
            || v.get("guaranteed").is_some()
            || v.get("ok") == Some(&Value::Bool(true)))
}

fn looks_like_self_describe_summary(summary: &str) -> bool {
    summary.contains("\"evolution\"")
        && summary.contains("may_propose_amendments")
        && (summary.contains("\"identity\"")
            || summary.contains("\"may_do\"")
            || summary.contains("\"guaranteed\"")
            || summary.starts_with('{'))
}

/// Format a stored operator-activity summary for TUI display (handles legacy raw JSON rows).
pub fn display_operator_activity_summary(tool_name: Option<&str>, summary: &str) -> String {
    if summary.starts_with("introspected ") {
        return summary.to_string();
    }
    if tool_name == Some("self_describe") || looks_like_self_describe_summary(summary) {
        if let Ok(v) = serde_json::from_str::<Value>(summary) {
            return summarize_self_describe(Some(&v));
        }
        return "introspected self".to_string();
    }
    if let Ok(v) = serde_json::from_str::<Value>(summary) {
        if looks_like_self_describe_json(&v) {
            return summarize_self_describe(Some(&v));
        }
    }
    summary.to_string()
}

fn summarize_tool_result(tool_name: &str, parsed: Option<&Value>, raw: &str) -> String {
    let summary = match tool_name {
        "workflow_wait" => summarize_workflow_wait(parsed),
        "workflow_state" => summarize_workflow_state(parsed),
        "artifact_build" | "artifact_inspect" => {
            let id = extract_field_str(parsed, &["artifact_id", "id"]).unwrap_or("artifact");
            format!("{} `{}`", tool_name, id)
        }
        "content_write" => format!(
            "wrote {}",
            extract_field_str(parsed, &["name", "sandbox_path", "handle"]).unwrap_or("content")
        ),
        "agent_spawn" => {
            let child = extract_field_str(parsed, &["child_session_id", "session_id"])
                .unwrap_or("child");
            let agent = extract_field_str(parsed, &["agent_id"]).unwrap_or("agent");
            format!("delegated to {} ({})", agent, child)
        }
        "web_search" => {
            let query = extract_field_str(parsed, &["query"]).unwrap_or("query");
            format!("search `{}`", truncate_chars(query, 80))
        }
        "web_fetch" => {
            let url = extract_field_str(parsed, &["url"]).unwrap_or("url");
            format!("fetch `{}`", truncate_chars(url, 80))
        }
        "sandbox_exec" => {
            let exit = parsed
                .and_then(|v| v.get("exit_code"))
                .and_then(|x| x.as_i64())
                .unwrap_or(-1);
            format!("command exit={}", exit)
        }
        "self_describe" => summarize_self_describe(parsed),
        _ => {
            if let Some(v) = parsed {
                if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
                    return truncate_chars(
                        &crate::log_redaction::redact_text_for_logs(msg),
                        SUMMARY_MAX_CHARS,
                    );
                }
            }
            truncate_chars(
                &crate::log_redaction::redact_text_for_logs(raw),
                SUMMARY_MAX_CHARS,
            )
        }
    };
    truncate_chars(&summary, SUMMARY_MAX_CHARS)
}

fn summarize_tool_error(tool_name: &str, parsed: Option<&Value>, raw: &str) -> String {
    if tool_name == "workflow_wait" {
        return truncate_chars(&summarize_workflow_wait(parsed), SUMMARY_MAX_CHARS);
    }
    if let Some(v) = parsed {
        if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
            return truncate_chars(
                &format!(
                    "{} failed: {}",
                    tool_name,
                    crate::log_redaction::redact_text_for_logs(msg)
                ),
                SUMMARY_MAX_CHARS,
            );
        }
        if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
            return truncate_chars(
                &format!(
                    "{} failed: {}",
                    tool_name,
                    crate::log_redaction::redact_text_for_logs(err)
                ),
                SUMMARY_MAX_CHARS,
            );
        }
    }
    truncate_chars(
        &format!(
            "{} failed: {}",
            tool_name,
            crate::log_redaction::redact_text_for_logs(raw)
        ),
        SUMMARY_MAX_CHARS,
    )
}

fn summarize_workflow_wait(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "workflow wait updated".to_string();
    };
    if v.get("join_satisfied").and_then(|x| x.as_bool()) == Some(true) {
        let waited = v
            .get("waited_secs")
            .and_then(|x| x.as_u64())
            .unwrap_or_default();
        return format!("workflow wait satisfied after {}s", waited);
    }
    if v.get("any_failed").and_then(|x| x.as_bool()) == Some(true) {
        let failed = v
            .get("failed_task_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        return format!("workflow wait: {} failed task(s)", failed);
    }
    "workflow wait updated".to_string()
}

fn summarize_workflow_state(parsed: Option<&Value>) -> String {
    let Some(v) = parsed else {
        return "workflow state refreshed".to_string();
    };
    let failed = v
        .get("failed_task_count")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let pending = v
        .get("pending_approvals")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!(
        "workflow state: {} pending approval, {} failed",
        pending, failed
    )
}

fn extract_field_str<'a>(parsed: Option<&'a Value>, keys: &[&str]) -> Option<&'a str> {
    let v = parsed?;
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_write_emits_progress() {
        let draft = classify_tool_activity(
            "content_write",
            r#"{"name":"news_fetcher.py"}"#,
            r#"{"ok":true,"name":"news_fetcher.py"}"#,
        )
        .expect("content_write should emit");
        assert_eq!(draft.kind, OperatorActivityKind::ToolCompleted);
        assert_eq!(draft.severity, OperatorActivitySeverity::Progress);
        assert!(draft.summary.contains("news_fetcher.py"));
    }

    #[test]
    fn execution_search_success_suppressed() {
        assert!(classify_tool_activity(
            "execution_search",
            r#"{"query":"%"}"#,
            r#"{"ok":true,"count":5}"#,
        )
        .is_none());
    }

    #[test]
    fn workflow_wait_failure_emits() {
        let draft = classify_tool_activity(
            "workflow_wait",
            "{}",
            r#"{"ok":true,"any_failed":true,"failed_task_count":1}"#,
        )
        .expect("failed wait should emit");
        assert_eq!(draft.severity, OperatorActivitySeverity::Error);
    }

    #[test]
    fn classify_workflow_event_plan_proposal() {
        let draft = classify_workflow_event("planframe.proposed").expect("should emit");
        assert_eq!(draft.kind, OperatorActivityKind::PlanProposal);
        assert_eq!(draft.severity, OperatorActivitySeverity::Attention);
    }

    #[test]
    fn classify_workflow_event_task_failed() {
        let draft = classify_workflow_event("task.failed").expect("should emit");
        assert_eq!(draft.kind, OperatorActivityKind::ToolFailed);
        assert_eq!(draft.severity, OperatorActivitySeverity::Error);
    }

    #[test]
    fn classify_workflow_event_unknown_returns_none() {
        assert!(classify_workflow_event("task.succeeded").is_none());
    }

    #[test]
    fn summaries_redact_aws_keys() {
        let draft = classify_tool_activity(
            "unknown_tool",
            r#"{}"#,
            r#"{"ok":true,"message":"wrote AWS_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE"}"#,
        )
        .expect("should emit");
        assert!(!draft.summary.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(draft.summary.contains("AWS_ACCESS_KEY="));
        assert!(draft.summary.contains("***REDACTED***"));
    }

    #[test]
    fn summaries_redact_bearer_tokens() {
        let draft = classify_tool_activity(
            "web_fetch",
            r#"{"url":"https://api.example.com"}"#,
            r#"{"ok":true,"url":"https://api.example.com","message":"fetched with Bearer eyJhbGciOiJIUzI1NiJ9.test.sig"}"#,
        )
        .expect("should emit");
        assert!(!draft.summary.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn summaries_truncate_long_content() {
        let long_msg = "a".repeat(500);
        let draft = classify_tool_activity(
            "content_write",
            r#"{"name":"big.txt"}"#,
            &format!(r#"{{"ok":true,"name":"big.txt","message":"{}"}}"#, long_msg),
        )
        .expect("should emit");
        assert!(draft.summary.len() <= 241);
    }

    #[test]
    fn self_describe_emits_compact_summary() {
        let raw = r#"{"ok":true,"identity":{"agent_id":"planner.collaborative","name":"Collaborative Planner"},"may_do":{"capabilities":[{"type":"AgentSpawn"}],"allowed_tool_tiers":["Core","Standard"]},"guaranteed":{"rights":[{"id":"Ri-0.1"},{"id":"Ri-0.2"}]},"evolution":{"may_propose_amendments":false,"paths":["skill promotion"]}}"#;
        let draft = classify_tool_activity("self_describe", "{}", raw).expect("self_describe should emit");
        assert!(draft.summary.starts_with("introspected Collaborative Planner"));
        assert!(draft.summary.contains("1 capabilities"));
        assert!(draft.summary.contains("2 rights"));
        assert!(draft.summary.contains("2 tool tiers"));
        assert!(!draft.summary.contains("evolution"));
    }

    #[test]
    fn display_operator_activity_summary_reformats_legacy_self_describe_json() {
        let legacy = r#"{"evolution":{"may_propose_amendments":false,"paths":["skill promotion"]}"#;
        let shown = display_operator_activity_summary(Some("self_describe"), legacy);
        assert_eq!(shown, "introspected self");
    }

    #[test]
    fn display_operator_activity_summary_reformats_full_self_describe_json() {
        let raw = r#"{"ok":true,"identity":{"name":"Planner"},"may_do":{"capabilities":[{},{}],"allowed_tool_tiers":["Core"]},"guaranteed":{"rights":[{}]},"evolution":{}}"#;
        let shown = display_operator_activity_summary(None, raw);
        assert_eq!(shown, "introspected Planner — 2 capabilities, 1 rights, 1 tool tiers");
    }

    #[test]
    fn format_self_describe_for_chat_lists_capabilities_and_tiers() {
        let raw = r#"{"ok":true,"identity":{"agent_id":"planner.collaborative","name":"Collaborative Planner"},"may_do":{"capabilities":[{"AgentSpawn":{"max_children":3,"max_spawn_depth":2}},{"ReadAccess":{"scopes":["*"]}}],"allowed_tool_tiers":["Core","Standard"]},"guaranteed":{"rights":[{"id":"Ri-0.1"}]},"evolution":{"paths":["skill promotion"]}}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let text = format_self_describe_for_chat(&v);
        assert!(text.contains("Collaborative Planner"));
        assert!(text.contains("AgentSpawn"));
        assert!(text.contains("ReadAccess"));
        assert!(text.contains("Core, Standard"));
        assert!(text.contains("skill promotion"));
    }

    /// The derived path shape (#818) renders per-path availability, and an
    /// unavailable path is shown with its reason rather than dropped.
    #[test]
    fn format_self_describe_for_chat_renders_derived_evolution_paths() {
        let raw = r#"{"ok":true,"identity":{"agent_id":"planner.default","name":"Planner"},"evolution":{"paths":[
            {"path":"agent_revision","available":true,"enacted_by":"self","via":["agent_revision_create_from_intent"]},
            {"path":"skill_crystallization","available":false,"enacted_by":"nothing","via":[],"unavailable_reason":"not implemented — tracked by #818"}
        ]}}"#;
        let v: Value = serde_json::from_str(raw).unwrap();
        let text = format_self_describe_for_chat(&v);
        assert!(text.contains("agent_revision — available"), "got:\n{text}");
        assert!(
            text.contains(
                "skill_crystallization — unavailable (not implemented — tracked by #818)"
            ),
            "got:\n{text}"
        );
    }

    #[test]
    fn classify_sentinel_notice_is_passive_advisory() {
        use crate::runtime::trajectory_health::{
            DivergenceSignal, DivergenceSignalKind, SignalSeverity,
        };
        let signals = vec![DivergenceSignal::new(
            DivergenceSignalKind::FeedbackIgnored,
            SignalSeverity::Critical,
            3.0,
            1.0,
        )
        .with_evidence("repeated output_schema violation")];
        let draft = classify_sentinel_notice("critical", "planner.default", 7, &signals);
        assert_eq!(draft.kind, OperatorActivityKind::SentinelNotice);
        assert_eq!(draft.severity, OperatorActivitySeverity::Error);
        assert!(draft.summary.contains("Sentinel [critical]"));
        assert!(draft.summary.contains("planner.default"));
        assert!(draft.summary.contains("turn 7"));
        assert!(draft.summary.contains("feedback_ignored"));
    }
}
