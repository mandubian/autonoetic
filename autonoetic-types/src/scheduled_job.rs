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

/// Parameters for `scheduled_jobs.list` — operator discovery of cron jobs
/// bound to root sessions (reconnect via `autonoetic room <root_session_id> --tui`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobsListParams {
    /// Filter by owning agent (e.g. `planner.default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_agent_id: Option<String>,
    /// Filter by root session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<String>,
    /// Filter by status (`active`, `paused`, `cancelled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Max entries to return. Clamped to 1..=500 by the gateway.
    #[serde(default = "default_scheduled_jobs_list_limit")]
    pub limit: u32,
}

fn default_scheduled_jobs_list_limit() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobsListResult {
    pub jobs: Vec<ScheduledJob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobTriggerEvent {
    pub event_id: String,
    pub job_id: String,
    pub workflow_id: String,
    pub task_id: String,
    pub root_session_id: String,
    pub triggered_at: String,
    pub scheduled_for: String,
}

/// Parameters for `scheduled_jobs.trigger` — manually fire a scheduled job's
/// agent now on the running gateway, bypassing the cron schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJobTriggerParams {
    /// The `job_id` of the scheduled job to fire.
    pub job_id: String,
    /// If true, skip the in-flight guard that prevents overlap with an
    /// already-running fire for the same job. Default false.
    #[serde(default)]
    pub force: bool,
}

/// Outcome of `scheduled_jobs.trigger`. Either the job was fired (a new task
/// enqueued) or skipped because a prior fire is still in flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ScheduledJobTriggerResult {
    /// A new task was enqueued for immediate execution.
    Triggered {
        #[serde(flatten)]
        event: ScheduledJobTriggerEvent,
    },
    /// An existing in-flight task was found for this job; no new task enqueued.
    TriggerSkipped {
        job_id: String,
        existing_task_id: String,
    },
}
