//! Evaluation types — eval suites, runs, and case results.
//!
//! Evaluations provide measurable evidence for agent revision promotion.
//! An eval suite defines test cases; an eval run executes those cases
//! against a specific agent revision and records structured results.

use serde::{Deserialize, Serialize};

/// Status of an eval run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunStatus {
    /// Queued but not yet started.
    Queued,
    /// Currently executing.
    Running,
    /// All cases passed.
    Passed,
    /// One or more cases failed.
    Failed,
    /// Cancelled by user or system.
    Cancelled,
}

/// Durable record of an eval suite definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalSuiteRecord {
    /// Unique suite ID (e.g., `suite-abc123`).
    pub suite_id: String,
    /// Display name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// Serialized suite spec (cases, assertions, etc.).
    pub spec_json: serde_json::Value,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Actor kind.
    pub created_by_type: String,
    /// Actor ID.
    pub created_by_id: String,
    /// Origin node for provenance.
    pub origin_node_id: String,
    /// Agent IDs this suite is intended to evaluate.
    /// The publishing agent must not appear in this list (ownership invariant).
    pub evaluated_targets: Vec<String>,
    /// Agent ID of the author (set by the gateway from the calling agent's manifest).
    pub author_agent_id: Option<String>,
    /// Suite ID this record supersedes (lineage link for versioned updates).
    pub based_on_suite_id: Option<String>,
}

/// Durable record of an eval run execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalRunRecord {
    /// Unique eval run ID (e.g., `eval-abc123`).
    pub eval_run_id: String,
    /// Suite this run belongs to.
    pub suite_id: String,
    /// Logical agent ID under test.
    pub subject_agent_id: String,
    /// Revision being evaluated.
    pub subject_revision_id: String,
    /// Optional baseline revision for comparison.
    pub baseline_revision_id: Option<String>,
    /// Current execution status.
    pub status: EvalRunStatus,
    /// RFC3339 queued timestamp.
    pub queued_at: String,
    /// RFC3339 started timestamp (None if not yet started).
    pub started_at: Option<String>,
    /// RFC3339 completed timestamp (None if not yet completed).
    pub completed_at: Option<String>,
    /// Rollup summary fields (JSON).
    pub summary_json: serde_json::Value,
    /// Content handle for the full eval report.
    pub report_handle: Option<String>,
    /// Origin node for provenance.
    pub origin_node_id: String,
}

/// Result of a single eval case within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalCaseResultRecord {
    /// Parent eval run ID.
    pub eval_run_id: String,
    /// Stable case ID within the suite.
    pub case_id: String,
    /// Case outcome: `passed`, `failed`, `error`.
    pub status: String,
    /// Optional numeric score.
    pub score: Option<f64>,
    /// Session ID if a session was spawned for this case.
    pub session_id: Option<String>,
    /// Short explanation of the outcome.
    pub notes: Option<String>,
    /// Serialized output from the case execution.
    pub output_json: serde_json::Value,
}
