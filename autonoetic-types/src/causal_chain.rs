//! Causal Chain log entry — immutable hash-chain audit trail.

use serde::{Deserialize, Serialize};

/// Status of a causal chain entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryStatus {
    Success,
    Denied,
    Error,
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "SUCCESS"),
            Self::Denied => write!(f, "DENIED"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// A single entry in the append-only `.jsonl` Causal Chain log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalChainEntry {
    pub timestamp: String,
    pub log_id: String,
    pub actor_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub event_seq: u64,
    pub category: String,
    pub action: String,
    pub target: Option<String>,
    pub status: EntryStatus,
    pub reason: Option<String>,
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub payload_hash: Option<String>,
    pub prev_hash: String,
    #[serde(default)]
    pub entry_hash: String,
}

pub const RULE_ID_EVENT_ATTRIBUTION: &str = "R+++3";

pub fn default_enforced_rules() -> Vec<String> {
    vec![RULE_ID_EVENT_ATTRIBUTION.to_string()]
}

/// Causal event record for storage in gateway.db causal_events table.
/// Matches the schema for queryable event storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEventRecord {
    pub event_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub event_seq: u64,
    pub timestamp: String,
    pub category: String,
    pub action: String,
    pub status: String,
    #[serde(default = "default_enforced_rules")]
    pub enforced_rules: Vec<String>,
    pub target: Option<String>,
    pub payload: Option<String>,
    pub payload_ref: Option<String>,
    pub evidence_ref: Option<String>,
    pub reason: Option<String>,
}

/// Whether this causal row should drive policy-decision notifications (hooks, chat TUI policy pane).
///
/// Semantics: `DENIED` / `ERROR` always; `SUCCESS` only when any enforced rule is not the baseline
/// attribution rule ([`RULE_ID_EVENT_ATTRIBUTION`]).
pub fn causal_event_notifies_policy_decision(event: &CausalEventRecord) -> bool {
    let s = event.status.as_str();
    if s.eq_ignore_ascii_case("DENIED") || s.eq_ignore_ascii_case("ERROR") {
        return true;
    }
    if s.eq_ignore_ascii_case("SUCCESS") {
        return event
            .enforced_rules
            .iter()
            .any(|r| r.as_str() != RULE_ID_EVENT_ATTRIBUTION);
    }
    false
}

/// Execution trace record for storage in gateway.db execution_traces table.
/// Stores structured tool execution results for agent learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTraceRecord {
    pub trace_id: String,
    pub event_id: Option<String>,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub tool_name: String,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: i64,
    pub success: i32,
    pub error_type: Option<String>,
    pub error_summary: Option<String>,
    pub approval_required: Option<i32>,
    pub approval_request_id: Option<String>,
    pub arguments: Option<String>,
    pub result: Option<String>,
}

impl ExecutionTraceRecord {
    pub fn redact_for_viewer(&self, viewer: super::disclosure::ViewerClass) -> Self {
        match viewer {
            super::disclosure::ViewerClass::Admin => self.clone(),
            super::disclosure::ViewerClass::Operator => {
                let mut out = self.clone();
                if let Some(ref args) = self.arguments {
                    out.arguments = Some(redact_json_string(args));
                }
                if let Some(ref result) = self.result {
                    out.result = Some(redact_json_string(result));
                }
                out
            }
            super::disclosure::ViewerClass::Agent => Self {
                trace_id: self.trace_id.clone(),
                event_id: self.event_id.clone(),
                agent_id: self.agent_id.clone(),
                session_id: self.session_id.clone(),
                turn_id: self.turn_id.clone(),
                timestamp: self.timestamp.clone(),
                tool_name: self.tool_name.clone(),
                command: self.command.clone(),
                exit_code: self.exit_code,
                stdout: None,
                stderr: None,
                duration_ms: self.duration_ms,
                success: self.success,
                error_type: self.error_type.clone(),
                error_summary: self.error_summary.clone(),
                approval_required: self.approval_required,
                approval_request_id: self.approval_request_id.clone(),
                arguments: None,
                result: None,
            },
        }
    }

    pub fn to_json_for_viewer(&self, viewer: super::disclosure::ViewerClass) -> serde_json::Value {
        let r = self.redact_for_viewer(viewer);
        serde_json::json!({
            "trace_id": r.trace_id,
            "agent_id": r.agent_id,
            "session_id": r.session_id,
            "turn_id": r.turn_id,
            "timestamp": r.timestamp,
            "tool_name": r.tool_name,
            "command": r.command,
            "exit_code": r.exit_code,
            "stdout": r.stdout,
            "stderr": r.stderr,
            "duration_ms": r.duration_ms,
            "success": r.success == 1,
            "error_type": r.error_type,
            "error_summary": r.error_summary,
            "approval_required": r.approval_required == Some(1),
            "approval_request_id": r.approval_request_id,
        })
    }
}

impl CausalEventRecord {
    pub fn redact_for_viewer(&self, viewer: super::disclosure::ViewerClass) -> Self {
        match viewer {
            super::disclosure::ViewerClass::Admin => self.clone(),
            super::disclosure::ViewerClass::Operator => {
                let mut out = self.clone();
                if let Some(ref payload) = self.payload {
                    out.payload = Some(redact_json_string(payload));
                }
                out
            }
            super::disclosure::ViewerClass::Agent => Self {
                event_id: self.event_id.clone(),
                agent_id: self.agent_id.clone(),
                session_id: self.session_id.clone(),
                turn_id: self.turn_id.clone(),
                event_seq: self.event_seq,
                timestamp: self.timestamp.clone(),
                category: self.category.clone(),
                action: self.action.clone(),
                status: self.status.clone(),
                enforced_rules: self.enforced_rules.clone(),
                target: self.target.clone(),
                payload: None,
                payload_ref: self.payload_ref.clone(),
                evidence_ref: None,
                reason: None,
            },
        }
    }
}

fn redact_json_string(s: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(v) => serde_json::to_string(&redact_json_value(&v)).unwrap_or_else(|_| "***REDACTED***".to_string()),
        Err(_) => {
            let lower = s.to_ascii_lowercase();
            if lower.contains("token")
                || lower.contains("secret")
                || lower.contains("authorization")
                || lower.contains("api_key")
                || lower.contains("apikey")
            {
                "***REDACTED***".to_string()
            } else {
                s.to_string()
            }
        }
    }
}

fn redact_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if is_sensitive_key(k) {
                    out.insert(k.clone(), serde_json::Value::String("***REDACTED***".to_string()));
                } else {
                    out.insert(k.clone(), redact_json_value(v));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_json_value).collect())
        }
        serde_json::Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() {
                let lower = t.to_ascii_lowercase();
                if lower.contains("bearer ")
                    || t.starts_with("sk-")
                    || t.contains("-----BEGIN")
                {
                    return serde_json::Value::String("***REDACTED***".to_string());
                }
            }
            serde_json::Value::String(s.clone())
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("secret")
        || k.contains("token")
        || k.contains("password")
        || k.contains("api_key")
        || k.contains("apikey")
        || k.contains("authorization")
        || k.contains("access_key")
        || k.contains("access_token")
        || k.contains("refresh_token")
        || k.contains("client_secret")
}

/// Session transcript record for storage in gateway.db session_transcripts table.
/// Used for full-text search across conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTranscriptRecord {
    pub transcript_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub agent_id: String,
    pub revision_id: Option<String>,
    pub user_id: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub turn_count: i64,
    pub transcript_handle: Option<String>,
    pub excerpt: Option<String>,
    pub origin_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedSessionReportRecord {
    pub root_session_id: String,
    pub report_handle: String,
    pub overview_handle: Option<String>,
    pub html_handle: Option<String>,
    pub narrative_handle: Option<String>,
    pub title: String,
    pub status: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub agent_count: i32,
    pub error_count: i32,
    pub approval_count: i32,
    pub search_text: String,
    pub generated_at: String,
    pub report_version: i32,
}
