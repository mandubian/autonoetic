//! Scheduled job types for cron-triggered task execution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledJobStatus {
    Active,
    Paused,
    Cancelled,
}

impl std::fmt::Display for ScheduledJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduledJobStatus::Active => write!(f, "active"),
            ScheduledJobStatus::Paused => write!(f, "paused"),
            ScheduledJobStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub job_id: String,
    pub owner_agent_id: String,
    pub root_session_id: String,
    pub target_agent_id: String,
    pub target_revision_id: String,
    pub message: String,
    pub metadata_json: Option<String>,
    pub cron_expr: String,
    pub timezone: String,
    pub next_run_at: String,
    pub last_run_at: Option<String>,
    pub status: ScheduledJobStatus,
    pub created_at: String,
    pub updated_at: String,
    pub last_error: Option<String>,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateScheduledJobRequest {
    pub agent_id: String,
    pub message: String,
    pub schedule_expr: String,
    pub target_agent_id: Option<String>,
    pub timezone: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobTriggerEvent {
    pub event_id: String,
    pub job_id: String,
    pub workflow_id: String,
    pub task_id: String,
    pub triggered_at: String,
    pub scheduled_for: String,
}
