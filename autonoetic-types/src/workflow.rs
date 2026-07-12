//! Durable workflow orchestration records (gateway-owned persistence).
//!
//! These types back the workflow layer described in `docs/archived/plan_workflow_orchestration.md`.
//! They intentionally avoid session-path parsing semantics — callers supply explicit
//! `root_session_id` and `workflow_id` at persistence boundaries.

use serde::{Deserialize, Serialize};

use crate::plan_frame::PlanRef;
use crate::task_completion::AgentOutcome;
use crate::tool_error::{FailureClass, RetryAdvice, SideEffectState};

fn default_true() -> bool {
    true
}

/// Lifecycle of a user-facing workflow run (one per root task / root session).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Active,
    WaitingChildren,
    BlockedApproval,
    Resumable,
    EmergencyStopping,
    EmergencyStopped,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkflowRunStatus::Active => "active",
            WorkflowRunStatus::WaitingChildren => "waiting_children",
            WorkflowRunStatus::BlockedApproval => "blocked_approval",
            WorkflowRunStatus::Resumable => "resumable",
            WorkflowRunStatus::EmergencyStopping => "emergency_stopping",
            WorkflowRunStatus::EmergencyStopped => "emergency_stopped",
            WorkflowRunStatus::Completed => "completed",
            WorkflowRunStatus::Failed => "failed",
            WorkflowRunStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::EmergencyStopped
        )
    }

    pub fn try_transition(self, next: WorkflowRunStatus) -> bool {
        use WorkflowRunStatus::*;
        if self == next {
            return true;
        }
        match (self, next) {
            (Completed | Failed | Cancelled | EmergencyStopped, _) => false,
            (EmergencyStopping, EmergencyStopped) => true,
            (EmergencyStopping, _) => false,
            (Active, _) => true,
            (WaitingChildren, _) => false,
            (BlockedApproval, Resumable | EmergencyStopping | Failed | Cancelled) => true,
            (BlockedApproval, _) => false,
            (Resumable, Active | EmergencyStopping | Completed | Failed | Cancelled) => true,
            _ => false,
        }
    }
}

impl Default for WorkflowRunStatus {
    fn default() -> Self {
        Self::Active
    }
}

/// Lifecycle of a delegated child execution unit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Pending,
    Runnable,
    Running,
    AwaitingApproval,
    Paused,
    Stale,
    Aborting,
    Aborted,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskRunStatus::Pending => "pending",
            TaskRunStatus::Runnable => "runnable",
            TaskRunStatus::Running => "running",
            TaskRunStatus::AwaitingApproval => "awaiting_approval",
            TaskRunStatus::Paused => "paused",
            TaskRunStatus::Stale => "stale",
            TaskRunStatus::Aborting => "aborting",
            TaskRunStatus::Aborted => "aborted",
            TaskRunStatus::Succeeded => "succeeded",
            TaskRunStatus::Failed => "failed",
            TaskRunStatus::Cancelled => "cancelled",
        }
    }

    /// Fully terminal — the task will never run again. `Stale` is **excluded**
    /// because it is a soft-timeout state: the operator can still approve the
    /// underlying request and resume the task (see #722 Stage 2 / P-2.11).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskRunStatus::Succeeded
                | TaskRunStatus::Failed
                | TaskRunStatus::Cancelled
                | TaskRunStatus::Aborted
        )
    }

    /// Terminal for join / wait purposes. Includes `Stale` so that a join
    /// observing a stale task unblocks (the task is either resolved or
    /// indefinitely abandoned — both unblock the join).
    pub fn is_terminal_for_join(self) -> bool {
        self.is_terminal() || self == TaskRunStatus::Stale
    }

    /// Resumable — the task can transition back to `Runnable`. Distinct from
    /// `is_terminal_for_join` because the orphan-reaper treats `Stale` as
    /// active-and-protected even though it unblocks joins.
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            TaskRunStatus::AwaitingApproval | TaskRunStatus::Paused | TaskRunStatus::Stale
        )
    }

    pub fn try_transition(self, next: TaskRunStatus) -> bool {
        use TaskRunStatus::*;
        if self == next {
            return true;
        }
        match (self, next) {
            (Succeeded | Failed | Aborted | Cancelled, _) => false,
            (Pending, Runnable | Cancelled) => true,
            (Pending, _) => false,
            (Runnable, Running | Cancelled | Failed) => true,
            (Runnable, _) => false,
            // Cancelled from any live state: operator/agent-driven
            // cancellation (workflow task_cancel, gate cancellation) is legal
            // while the task is Running/parked (#747 review).
            (Running, AwaitingApproval | Paused | Aborting | Succeeded | Failed | Cancelled) => {
                true
            }
            (Running, _) => false,
            // AwaitingApproval can be resolved by an approval (→ Succeeded),
            // rejected (→ Failed), aborted (→ Aborting → Aborted), timed out
            // (→ Stale), or cancelled. Amend (→ Runnable) resumes for re-gating.
            (AwaitingApproval, Runnable | Aborting | Failed | Stale | Succeeded | Cancelled) => {
                true
            }
            (AwaitingApproval, _) => false,
            // Stale is resumable: a late approval revives it (→ Runnable), a
            // late rejection fails it (→ Failed, via unblock/fan-in), and an
            // operator can still cancel it.
            (Stale, Runnable | Failed | Cancelled) => true,
            (Stale, _) => false,
            (Paused, Runnable | Aborting | Failed | Cancelled) => true,
            (Paused, _) => false,
            (Aborting, Aborted) => true,
            (Aborting, _) => false,
        }
    }
}

impl Default for TaskRunStatus {
    fn default() -> Self {
        Self::Pending
    }
}

/// One durable workflow run keyed by `workflow_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowRun {
    pub workflow_id: String,
    pub root_session_id: String,
    /// Front-door / lead agent when known; empty if not yet recorded.
    #[serde(default)]
    pub lead_agent_id: String,
    #[serde(default)]
    pub status: WorkflowRunStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub active_task_ids: Vec<String>,
    /// Task IDs currently queued for async execution (not yet running).
    #[serde(default)]
    pub queued_task_ids: Vec<String>,
    /// Join policy for this workflow's planner resume.
    #[serde(default)]
    pub join_policy: JoinPolicy,
    /// Task IDs that must complete before the planner resumes (join condition).
    #[serde(default)]
    pub join_task_ids: Vec<String>,
    /// Reference to the active PlanFrame governing this workflow, if any.
    #[serde(default)]
    pub active_plan_ref: Option<PlanRef>,
    /// True when a terminal workflow was transiently reactivated by the root
    /// planner to allow follow-up work (e.g. spawning a freshly built agent).
    /// While true, only the root session may spawn agents; child-session spawns
    /// remain blocked.
    #[serde(default)]
    pub reactivated_for_root_spawn: bool,
}

/// One delegated task (typically one `agent.spawn` child path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskRun {
    pub task_id: String,
    pub workflow_id: String,
    /// Target specialist `agent_id`.
    pub agent_id: String,
    /// Child delegation session id (e.g. `root/parent-abc`).
    pub session_id: String,
    /// Session id of the delegating agent when spawned from a parent.
    #[serde(default)]
    pub parent_session_id: String,
    #[serde(default)]
    pub status: TaskRunStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub source_agent_id: Option<String>,
    /// Short summary (length-capped by the gateway), not a full transcript.
    #[serde(default)]
    pub result_summary: Option<String>,
    /// Join group this task belongs to (tasks in the same group are awaited together).
    #[serde(default)]
    pub join_group: Option<String>,
    /// Original kickoff message for the child agent. Preserved across approval boundaries.
    #[serde(default)]
    pub message: Option<String>,
    /// Original metadata passed through to the child. Preserved across approval boundaries.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Stage-local retry count used by the workflow layer.
    #[serde(default)]
    pub retry_count: u32,
    /// Last gateway-owned failure classification observed for this task.
    #[serde(default)]
    pub last_failure_class: Option<FailureClass>,
    /// Retry policy attached to the logical stage execution.
    #[serde(default)]
    pub retry_policy: Option<serde_json::Value>,
    /// Side-effect state carried forward across retries/resume.
    #[serde(default)]
    pub side_effect_state: Option<SideEffectState>,
    /// Stable dedupe key for durable operations.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Structured child-state wake-up payload for parent workflow resumption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChildStateNotification {
    pub workflow_id: String,
    pub task_id: String,
    pub child_session_id: String,
    pub child_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_conflict_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_advice: Option<RetryAdvice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect_state: Option<SideEffectState>,
    /// Normalized child outcome parsed from the child's final reply
    /// (RFC #775 Part A). When `Some(ClarificationNeeded)`, the child is
    /// requesting clarification — penalty-free, not a failure. Parents
    /// branch on this mechanically instead of re-deriving it from `summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_outcome: Option<AgentOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Join policy for a group of tasks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JoinPolicy {
    /// All tasks in the group must complete before the planner resumes.
    AllOf,
    /// Any one task completing satisfies the join.
    AnyOf,
    /// First task that succeeds satisfies the join; failures are ignored.
    FirstSuccess,
    /// Manual: planner must explicitly call workflow.wait or resume.
    Manual,
}

impl Default for JoinPolicy {
    fn default() -> Self {
        Self::AllOf
    }
}

/// A queued task awaiting async execution by the scheduler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedTaskRun {
    pub task_id: String,
    pub workflow_id: String,
    pub agent_id: String,
    /// Kickoff message for the child agent.
    pub message: String,
    /// Delegation path used as session_id for the child.
    pub child_session_id: String,
    /// Source (parent) session.
    pub parent_session_id: String,
    /// Agent that initiated the spawn.
    pub source_agent_id: String,
    /// Optional metadata passed through to the child.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Join group for this task (planner resumes when join condition is met).
    #[serde(default)]
    pub join_group: Option<String>,
    /// Whether this task blocks the planner from continuing.
    #[serde(default = "default_true")]
    pub blocks_planner: bool,
    pub enqueued_at: String,
    /// Spawn-time credential bindings: maps service name to credential_id.
    /// These override runtime.lock resolution for the child agent.
    #[serde(default)]
    pub credential_bindings: Vec<crate::runtime_lock::LockedCredentialMount>,
}

/// Append-only workflow event (mirrors plan `WorkflowEvent` concept).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowEventRecord {
    pub event_id: String,
    pub workflow_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub event_type: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub occurred_at: String,
}

// ---------------------------------------------------------------------------
// Durable checkpoints (Phase 3)
// ---------------------------------------------------------------------------

/// Durable planner-level checkpoint.
/// Stores the orchestrator's delegation state at the end of a turn so it can
/// resume deterministically after join satisfaction or gateway restart.
/// Explicitly separate from `SessionContext` (prompt continuity) and
/// `SessionSnapshot` (branch/fork).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowCheckpoint {
    pub workflow_id: String,
    /// Monotonically increasing version per workflow.
    pub version: u32,
    /// Natural-language description of what the planner was doing.
    pub planner_intent: String,
    /// Task IDs the planner delegated and is waiting for.
    pub pending_task_ids: Vec<String>,
    /// The join policy governing resume.
    pub join_policy: JoinPolicy,
    /// Arbitrary planner context (JSON): delegation instructions, expected
    /// result shape, intermediate decisions, etc.
    #[serde(default)]
    pub context: serde_json::Value,
    pub created_at: String,
}

/// Durable task-level checkpoint.
/// Stores a child task's execution progress between sandbox execs or approval
/// boundaries so it can resume without replaying from scratch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskCheckpoint {
    pub workflow_id: String,
    pub task_id: String,
    /// Monotonically increasing version per task.
    pub version: u32,
    /// Label for the current execution step (e.g., "setup", "run_tests", "build").
    pub step: String,
    /// Arbitrary task state (JSON): last script output, file hashes,
    /// accumulated data, etc.
    #[serde(default)]
    pub state: serde_json::Value,
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::TaskRunStatus;

    #[test]
    fn is_terminal_includes_succeeded_failed_cancelled_aborted() {
        assert!(TaskRunStatus::Succeeded.is_terminal());
        assert!(TaskRunStatus::Failed.is_terminal());
        assert!(TaskRunStatus::Cancelled.is_terminal());
        assert!(TaskRunStatus::Aborted.is_terminal());
    }

    #[test]
    fn is_terminal_excludes_stale_and_active_states() {
        // `Stale` is resumable per #722 Stage 2 / P-2.11.
        assert!(!TaskRunStatus::Stale.is_terminal());
        // Active states.
        assert!(!TaskRunStatus::Pending.is_terminal());
        assert!(!TaskRunStatus::Runnable.is_terminal());
        assert!(!TaskRunStatus::Running.is_terminal());
        assert!(!TaskRunStatus::AwaitingApproval.is_terminal());
        assert!(!TaskRunStatus::Paused.is_terminal());
        assert!(!TaskRunStatus::Aborting.is_terminal());
    }

    #[test]
    fn is_terminal_for_join_includes_stale() {
        assert!(TaskRunStatus::Succeeded.is_terminal_for_join());
        assert!(TaskRunStatus::Failed.is_terminal_for_join());
        assert!(TaskRunStatus::Cancelled.is_terminal_for_join());
        assert!(TaskRunStatus::Aborted.is_terminal_for_join());
        assert!(TaskRunStatus::Stale.is_terminal_for_join());
    }

    #[test]
    fn is_terminal_for_join_excludes_active_states() {
        assert!(!TaskRunStatus::Pending.is_terminal_for_join());
        assert!(!TaskRunStatus::Runnable.is_terminal_for_join());
        assert!(!TaskRunStatus::Running.is_terminal_for_join());
        assert!(!TaskRunStatus::AwaitingApproval.is_terminal_for_join());
        assert!(!TaskRunStatus::Paused.is_terminal_for_join());
        assert!(!TaskRunStatus::Aborting.is_terminal_for_join());
    }

    #[test]
    fn is_resumable_includes_awaiting_paused_stale() {
        assert!(TaskRunStatus::AwaitingApproval.is_resumable());
        assert!(TaskRunStatus::Paused.is_resumable());
        assert!(TaskRunStatus::Stale.is_resumable());
    }

    #[test]
    fn is_resumable_excludes_terminal_and_active_states() {
        assert!(!TaskRunStatus::Succeeded.is_resumable());
        assert!(!TaskRunStatus::Failed.is_resumable());
        assert!(!TaskRunStatus::Cancelled.is_resumable());
        assert!(!TaskRunStatus::Aborted.is_resumable());
        assert!(!TaskRunStatus::Pending.is_resumable());
        assert!(!TaskRunStatus::Runnable.is_resumable());
        assert!(!TaskRunStatus::Running.is_resumable());
        assert!(!TaskRunStatus::Aborting.is_resumable());
    }

    #[test]
    fn try_transition_refuses_illegal_terminus_to_active() {
        // No transition out of a terminal state.
        for terminal in [
            TaskRunStatus::Succeeded,
            TaskRunStatus::Failed,
            TaskRunStatus::Aborted,
            TaskRunStatus::Cancelled,
        ] {
            for target in [
                TaskRunStatus::Pending,
                TaskRunStatus::Runnable,
                TaskRunStatus::Running,
                TaskRunStatus::AwaitingApproval,
                TaskRunStatus::Paused,
                TaskRunStatus::Stale,
            ] {
                assert!(
                    !terminal.try_transition(target),
                    "terminal {:?} should not transition to {:?}",
                    terminal,
                    target
                );
            }
        }
    }

    #[test]
    fn try_transition_allows_stale_to_runnable() {
        assert!(TaskRunStatus::Stale.try_transition(TaskRunStatus::Runnable));
    }

    /// #747 review: operator/agent-driven cancellation (workflow task_cancel,
    /// gate cancellation) must be legal from every live state — otherwise the
    /// enforcement in update_task_run_status turns a cancel into a silent
    /// no-op and the task is stuck live forever.
    #[test]
    fn try_transition_allows_cancel_from_every_live_state() {
        for live in [
            TaskRunStatus::Pending,
            TaskRunStatus::Runnable,
            TaskRunStatus::Running,
            TaskRunStatus::AwaitingApproval,
            TaskRunStatus::Paused,
            TaskRunStatus::Stale,
        ] {
            assert!(
                live.try_transition(TaskRunStatus::Cancelled),
                "cancellation from live state {:?} must be legal",
                live
            );
        }
    }

    /// #747 review: a Stale task's approval can still be *rejected* late —
    /// unblock/fan-in then drive Stale → Failed, which must be legal.
    #[test]
    fn try_transition_allows_stale_to_failed_on_late_rejection() {
        assert!(TaskRunStatus::Stale.try_transition(TaskRunStatus::Failed));
        // But a Stale task never jumps straight to Succeeded or Running.
        assert!(!TaskRunStatus::Stale.try_transition(TaskRunStatus::Succeeded));
        assert!(!TaskRunStatus::Stale.try_transition(TaskRunStatus::Running));
    }

    #[test]
    fn try_transition_allows_same_to_same() {
        for s in [
            TaskRunStatus::Pending,
            TaskRunStatus::Runnable,
            TaskRunStatus::Running,
            TaskRunStatus::AwaitingApproval,
            TaskRunStatus::Paused,
            TaskRunStatus::Stale,
            TaskRunStatus::Aborting,
            TaskRunStatus::Aborted,
            TaskRunStatus::Succeeded,
            TaskRunStatus::Failed,
            TaskRunStatus::Cancelled,
        ] {
            assert!(s.try_transition(s));
        }
    }
}
