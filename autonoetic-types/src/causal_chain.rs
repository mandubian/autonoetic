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
