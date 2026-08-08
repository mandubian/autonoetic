//! Durable workflow / task persistence under the gateway scheduler directory.
//!
//! Layout (under `<agents_dir>/.gateway/scheduler/workflows/`):
//! - `index/by_root/<sha256-hex>.json` — maps stable root key → `workflow_id`
//! - `runs/<workflow_id>/workflow.json` — [`WorkflowRun`](autonoetic_types::workflow::WorkflowRun)
//! - `runs/<workflow_id>/tasks/<task_id>.json` — legacy file fallback (migrated to SQLite on first read)
//! - TaskRun, queued task, and task-claim state are now stored in SQLite (`gateway.db`)
//!   tables `task_runs`, `queued_task_runs`, and `task_claims`.
//! - Workflow events are stored in SQLite table `workflow_events` (`gateway.db`).

use crate::execution::gateway_root_dir;
use crate::runtime::failure_classification::{
    classify_task_status, metadata_for_failure_class, WorkflowFailureMetadata,
};
use crate::runtime::live_digest::base_session_id;
use crate::scheduler::gateway_store::{GatewayStore, TaskExecutionClaim};
use crate::scheduler::store::{read_json_file, write_json_file};
use autonoetic_types::agent::SessionLifecycleState;
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::{FailureClass, RetryAdvice, SideEffectState};
use autonoetic_types::workflow::{
    ChildStateNotification, QueuedTaskRun, TaskCheckpoint, TaskRun, TaskRunStatus,
    WorkflowCheckpoint, WorkflowEventRecord, WorkflowRun, WorkflowRunStatus,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RootWorkflowIndex {
    workflow_id: String,
    root_session_id: String,
}

/// Approval metadata to include in `task.awaiting_approval` events.
#[derive(Debug, Clone, Default)]
pub struct ApprovalMetadata {
    /// The approval request ID (e.g., "apr-1234abcd")
    pub request_id: String,
    /// The kind of approval (e.g., "sandbox", "agent_install", "tool_execution")
    pub kind: String,
    /// Human-readable reason for the approval (if available)
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct StageRetryDecision {
    pub failure: Option<WorkflowFailureMetadata>,
    pub retry_scheduled: bool,
    pub budget_exhausted: bool,
    pub next_retry_count: u32,
    pub max_retries: Option<u32>,
}

pub(crate) fn retry_policy_from_metadata(
    metadata: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    metadata
        .and_then(|value| value.as_object())
        .and_then(|object| object.get("retry_policy"))
        .cloned()
}

/// Gateway default retry policy for spawned child tasks when the spawner did
/// not provide one. Only genuinely transient failure kinds auto-retry —
/// everything else requires parent action (RFC #775 Part A taxonomy):
///
/// | FailureClass          | Auto-retry? | Rationale                                  |
/// |-----------------------|-------------|--------------------------------------------|
/// | `transient_infra`     | 1 attempt   | Provider 5xx, rate limit — likely recovers |
/// | `timeout`             | 1 attempt   | May be a transient slow-down               |
/// | `schema_validation_*` | 0 (parent)  | Parent should repair payload               |
/// | `policy_denied`       | 0 (parent)  | Capability gap — re-delegate/escalate      |
/// | `output_contract_*`   | 0 (parent)  | No blind retry (RFC invariant 1)           |
/// | `child_gave_up`       | 0 (parent)  | Silent failure — parent must investigate   |
/// | `install_conflict`    | 0 (parent)  | State conflict — operator resolution       |
/// | all others            | 0           | Unknown — escalate to parent               |
pub(crate) fn default_child_retry_policy() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "transient_infra": { "max_retries": 1 },
        "timeout": { "max_retries": 1 }
    }))
}

fn retry_budget_for_failure(
    retry_policy: &serde_json::Value,
    failure_class: FailureClass,
) -> Option<u32> {
    let key = serde_json::to_value(failure_class)
        .ok()?
        .as_str()?
        .to_string();
    retry_policy
        .get(&key)
        .and_then(|entry| entry.get("max_retries"))
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

pub(crate) fn evaluate_stage_retry(
    task: &TaskRun,
    status: TaskRunStatus,
    result_summary: Option<&str>,
) -> StageRetryDecision {
    let mut failure = task.last_failure_class.map(|failure_class| {
        let mut metadata = metadata_for_failure_class(failure_class);
        if task.side_effect_state.is_some() {
            metadata.side_effect_state = task.side_effect_state;
        }
        metadata
    });
    if failure.is_none() {
        failure = classify_task_status(status, result_summary);
    }
    let mut decision = StageRetryDecision {
        failure: failure.clone(),
        retry_scheduled: false,
        budget_exhausted: false,
        next_retry_count: task.retry_count,
        max_retries: None,
    };

    if status != TaskRunStatus::Failed {
        return decision;
    }

    let Some(mut failure) = failure else {
        return decision;
    };
    let Some(retry_policy) = task.retry_policy.as_ref() else {
        decision.failure = Some(failure);
        return decision;
    };
    let Some(failure_class) = failure.failure_class else {
        decision.failure = Some(failure);
        return decision;
    };
    let Some(max_retries) = retry_budget_for_failure(retry_policy, failure_class) else {
        decision.failure = Some(failure);
        return decision;
    };
    decision.max_retries = Some(max_retries);

    if failure.retryable != Some(true)
        || failure.requires_external_event == Some(true)
        || failure.requires_human == Some(true)
    {
        decision.failure = Some(failure);
        return decision;
    }

    match failure.side_effect_state {
        Some(SideEffectState::Unknown) => {
            failure.retry_advice = Some(RetryAdvice::EscalateHuman);
            failure.retryable = Some(false);
        }
        Some(SideEffectState::Committed) => {
            failure.retry_advice = Some(RetryAdvice::DoNotRetry);
            failure.retryable = Some(false);
        }
        _ => {
            if task.retry_count < max_retries {
                failure.retry_advice = Some(RetryAdvice::RetrySameStage);
                decision.retry_scheduled = true;
                decision.next_retry_count = task.retry_count + 1;
            } else {
                failure.retry_advice = Some(RetryAdvice::DoNotRetry);
                failure.retryable = Some(false);
                decision.budget_exhausted = true;
            }
        }
    }

    decision.failure = Some(failure);
    decision
}

pub fn workflows_root(config: &GatewayConfig) -> PathBuf {
    gateway_root_dir(config).join("scheduler").join("workflows")
}

fn index_dir(config: &GatewayConfig) -> PathBuf {
    workflows_root(config).join("index").join("by_root")
}

fn root_index_path(config: &GatewayConfig, root_session_id: &str) -> PathBuf {
    index_dir(config).join(format!("{}.json", root_session_key(root_session_id)))
}

fn root_session_key(root_session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root_session_id.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn workflow_run_dir(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflows_root(config).join("runs").join(workflow_id)
}

pub fn workflow_run_path(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id).join("workflow.json")
}

pub fn task_run_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id)
        .join("tasks")
        .join(format!("{task_id}.json"))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn new_workflow_id() -> String {
    autonoetic_types::id_format::short_random_id("wf-")
}

/// Allocate a new `task-*` id (separate from session paths).
pub fn new_task_id() -> String {
    autonoetic_types::id_format::short_random_id("task-")
}

fn new_event_id() -> String {
    autonoetic_types::id_format::short_random_id("wevt-")
}

/// Load a workflow run by id, if present.
pub fn load_workflow_run(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowRun>> {
    let p = workflow_run_path(config, workflow_id);
    if !p.exists() {
        return Ok(None);
    }
    Ok(Some(read_json_file(&p)?))
}

pub fn is_workflow_terminal(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let run = match load_workflow_run(config, store, workflow_id)? {
        Some(r) => r,
        None => return Ok(false),
    };
    Ok(matches!(
        run.status,
        WorkflowRunStatus::Completed
            | WorkflowRunStatus::Failed
            | WorkflowRunStatus::Cancelled
            | WorkflowRunStatus::EmergencyStopped
    ))
}

/// Mark pending notifications for a workflow as Suppressed so they do not wake
/// the root session after the workflow has reached a terminal state.
pub fn suppress_pending_notifications_for_workflow(
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<usize> {
    let Some(store) = store else {
        return Ok(0);
    };
    store.suppress_pending_notifications_for_workflow(workflow_id)
}

/// Resolve `wf-*` id from a root session id (`agent.spawn` root), if an index exists.
pub fn resolve_workflow_id_for_root_session(
    config: &GatewayConfig,
    root_session_id: &str,
) -> anyhow::Result<Option<String>> {
    // Check SQLite first (more reliable than file system)
    let gateway_dir = crate::execution::gateway_root_dir(config);
    match crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir) {
        Ok(store) => {
            match store.resolve_workflow_id(root_session_id) {
                Ok(Some(wf_id)) => return Ok(Some(wf_id)),
                Ok(None) => {} // Not in SQLite, try file
                Err(e) => {
                    tracing::debug!(target: "workflow_store", error = %e, "SQLite workflow index lookup failed, falling back to file");
                }
            }
        }
        Err(e) => {
            tracing::debug!(target: "workflow_store", error = %e, "Failed to open GatewayStore for workflow resolution, falling back to file");
        }
    }
    // Fallback to file
    let p = root_index_path(config, root_session_id);
    tracing::debug!(
        target: "workflow_store",
        path = %p.display(),
        root_session_id = %root_session_id,
        "resolve_workflow_id_for_root_session: checking file fallback"
    );
    if !p.exists() {
        tracing::debug!(target: "workflow_store", "resolve_workflow_id_for_root_session: file does not exist");
        return Ok(None);
    }
    let idx: RootWorkflowIndex = read_json_file(&p)?;
    tracing::debug!(target: "workflow_store", workflow_id = %idx.workflow_id, "resolve_workflow_id_for_root_session: found via file");
    Ok(Some(idx.workflow_id))
}

/// Load append-only workflow events from SQLite (`workflow_events`).
pub fn load_workflow_events(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<WorkflowEventRecord>> {
    let mut owned_store = None;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = Some(crate::scheduler::gateway_store::GatewayStore::open(
                &gateway_dir
            )?);
            owned_store.as_ref().unwrap()
        }
    };

    let mut events = store.list_workflow_events(workflow_id)?;
    tracing::debug!(
        target: "workflow_store",
        workflow_id = %workflow_id,
        event_count = events.len(),
        "load_workflow_events: SQLite source"
    );

    // Ensure deterministic ordering for callers.
    events.sort_by(|a, b| {
        a.occurred_at
            .cmp(&b.occurred_at)
            .then_with(|| a.event_id.cmp(&b.event_id))
    });
    Ok(events)
}

/// Persist full workflow run (creates parent dirs).
pub fn save_workflow_run(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    run: &WorkflowRun,
) -> anyhow::Result<()> {
    let path = workflow_run_path(config, &run.workflow_id);
    write_json_file(&path, run)
}

/// Create or load the [`WorkflowRun`] for a root session (one workflow per root).
///
/// `lead_agent_id`, when `Some`, is written on first creation or used to fill an empty
/// `lead_agent_id` on an existing record.
pub fn ensure_workflow_for_root_session(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
    lead_agent_id: Option<&str>,
) -> anyhow::Result<WorkflowRun> {
    anyhow::ensure!(
        !root_session_id.trim().is_empty(),
        "root_session_id must not be empty"
    );

    let idx_path = root_index_path(config, root_session_id);
    if idx_path.exists() {
        let idx: RootWorkflowIndex = read_json_file(&idx_path)?;
        let mut run: WorkflowRun = match load_workflow_run(config, store, &idx.workflow_id)? {
            Some(r) => r,
            None => {
                // Index exists but run file missing — recreate minimal run
                WorkflowRun {
                    workflow_id: idx.workflow_id.clone(),
                    root_session_id: root_session_id.to_string(),
                    lead_agent_id: lead_agent_id.unwrap_or("").to_string(),
                    status: WorkflowRunStatus::Active,
                    created_at: now_rfc3339(),
                    updated_at: now_rfc3339(),
                    active_task_ids: vec![],
                    queued_task_ids: vec![],
                    join_policy: Default::default(),
                    join_task_ids: vec![],
                    active_plan_ref: None,
                    reactivated_for_root_spawn: false,
                }
            }
        };
        if run.lead_agent_id.is_empty() {
            if let Some(lead) = lead_agent_id.filter(|s| !s.is_empty()) {
                run.lead_agent_id = lead.to_string();
                run.updated_at = now_rfc3339();
                save_workflow_run(config, store, &run)?;
            }
        }
        return Ok(run);
    }

    let workflow_id = new_workflow_id();
    let ts = now_rfc3339();
    let run = WorkflowRun {
        workflow_id: workflow_id.clone(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: lead_agent_id.unwrap_or("").to_string(),
        status: WorkflowRunStatus::Active,
        created_at: ts.clone(),
        updated_at: ts,
        active_task_ids: vec![],
        queued_task_ids: vec![],
        join_policy: Default::default(),
        join_task_ids: vec![],
        active_plan_ref: None,
        reactivated_for_root_spawn: false,
    };

    save_workflow_run(config, store, &run)?;
    write_json_file(
        &idx_path,
        &RootWorkflowIndex {
            workflow_id: workflow_id.clone(),
            root_session_id: root_session_id.to_string(),
        },
    )?;
    // Also store in SQLite for reliable lookups
    if let Some(s) = store {
        if let Err(e) = s.set_workflow_index(root_session_id, &workflow_id) {
            tracing::warn!(
                target: "workflow_store",
                root_session_id = %root_session_id,
                workflow_id = %workflow_id,
                error = %e,
                "Failed to set workflow index in SQLite"
            );
        }
    };

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.clone(),
            task_id: None,
            event_type: "workflow.started".to_string(),
            agent_id: None,
            payload: serde_json::json!({
                "root_session_id": root_session_id,
                "lead_agent_id": run.lead_agent_id,
            }),
            occurred_at: now_rfc3339(),
        },
    )?;

    crate::scheduler::workflow_causal::mirror_orchestration_event(
        config,
        root_session_id,
        "workflow.started",
        EntryStatus::Success,
        serde_json::json!({
            "workflow_id": workflow_id,
            "root_session_id": root_session_id,
            "lead_agent_id": run.lead_agent_id,
        }),
    );

    Ok(run)
}

fn workflow_session_dir(config: &GatewayConfig, root_session_id: &str) -> PathBuf {
    gateway_root_dir(config)
        .join("sessions")
        .join(base_session_id(root_session_id))
}

fn workflow_run_status_snake(s: WorkflowRunStatus) -> &'static str {
    match s {
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

fn task_run_status_snake(s: TaskRunStatus) -> &'static str {
    match s {
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

/// Rewrite `.gateway/sessions/{root}/workflow_graph.md` from current workflow + task + event state.
///
/// Called after each workflow event append so operators can open it beside `digest.md`.
pub fn refresh_workflow_graph_markdown(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let run = match load_workflow_run(config, None, workflow_id)? {
        Some(r) => r,
        None => return Ok(()),
    };
    let tasks = list_task_runs_for_workflow(config, None, workflow_id)?;
    let events = load_workflow_events(config, store, workflow_id)?;
    let start = events.len().saturating_sub(16);
    let recent = &events[start..];

    let dir = workflow_session_dir(config, &run.root_session_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join("workflow_graph.md");

    let mut body = String::new();
    writeln!(body, "# Workflow graph: `{}`", run.root_session_id)?;
    writeln!(body)?;
    writeln!(
        body,
        "_Auto-updated when workflow orchestration events append to the gateway store._"
    )?;
    writeln!(body)?;
    writeln!(body, "| Field | Value |")?;
    writeln!(body, "|-------|-------|")?;
    writeln!(body, "| workflow_id | `{}` |", run.workflow_id)?;
    writeln!(
        body,
        "| status | `{}` |",
        workflow_run_status_snake(run.status)
    )?;
    let lead = if run.lead_agent_id.is_empty() {
        "_(unknown)_"
    } else {
        run.lead_agent_id.as_str()
    };
    writeln!(body, "| lead (planner) | `{}` |", lead)?;
    writeln!(body)?;
    writeln!(body, "## Tasks")?;
    writeln!(body)?;
    if tasks.is_empty() {
        writeln!(body, "_(none yet)_")?;
    } else {
        for t in &tasks {
            writeln!(
                body,
                "- **{}** · `{}` · _{}_ — `{}`",
                t.agent_id,
                t.task_id,
                task_run_status_snake(t.status),
                t.session_id
            )?;
        }
    }
    writeln!(body)?;
    writeln!(body, "## Recent workflow events")?;
    writeln!(body)?;
    if recent.is_empty() {
        writeln!(body, "_(none)_")?;
    } else {
        for e in recent {
            let tid = e.task_id.as_deref().unwrap_or("—");
            let ts_short: String = e.occurred_at.chars().take(22).collect();
            writeln!(
                body,
                "- `{}` · **{}** · task `{}`",
                ts_short, e.event_type, tid
            )?;
        }
    }
    writeln!(body)?;
    writeln!(body, "---")?;
    writeln!(
        body,
        "_Generated: {} (UTC)_",
        chrono::Utc::now().to_rfc3339()
    )?;

    fs::write(&path, body)?;
    Ok(())
}

/// Append one event to the workflow's SQLite store (`workflow_events`).
pub fn append_workflow_event(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    event: &WorkflowEventRecord,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = Some(crate::scheduler::gateway_store::GatewayStore::open(
                &gateway_dir
            )?);
            owned_store.as_ref().unwrap()
        }
    };
    store.append_workflow_event(event)?;
    tracing::debug!(
        target: "workflow_store",
        workflow_id = %event.workflow_id,
        event_id = %event.event_id,
        event_type = %event.event_type,
        "append_workflow_event: appended to SQLite"
    );
    if let Some(draft) =
        crate::runtime::operator_activity::classify_workflow_event(&event.event_type)
    {
        let root_session_id = store
            .resolve_root_session_id(&event.workflow_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let record = draft.into_record(
            root_session_id.clone(),
            String::new(),
            event.agent_id.clone().unwrap_or_default(),
            Some(event.workflow_id.clone()),
            event.task_id.clone(),
            None,
            None,
            None,
            Some(event.event_id.clone()),
        );
        let rate_limit = config.operator_activity.rate_limit_per_min;
        if let Err(e) = store.insert_operator_activity_throttled(&record, rate_limit) {
            tracing::debug!(
                target: "operator_activity",
                error = %e,
                "Failed to persist workflow event operator activity"
            );
        }
    }
    if let Err(e) = refresh_workflow_graph_markdown(config, Some(store), &event.workflow_id) {
        tracing::warn!(
            target: "session_timeline",
            workflow_id = %event.workflow_id,
            error = %e,
            "Failed to refresh workflow_graph.md"
        );
    }
    maybe_emit_workflow_timeline(config, store, event);
    Ok(())
}

/// Mirror workflow events onto the canonical session timeline so the Session
/// Room (and any channel reading `live_digest_events`) shows trigger + result
/// lines — not just the chat TUI's workflow-event poll.
///
/// Admits two event sources:
/// - **Scheduled jobs**: workflows whose id starts with `sched-` (the cron
///   path). Maps `scheduled_job.triggered` / `task.completed` / `task.failed`
///   to `scheduled_job.*` timeline types.
/// - **Manual `/curate`**: tasks whose id starts with `curate-` (the
///   `curation.run_for_session` RPC path at router.rs). Maps `task.completed`
///   / `task.failed` to `curation.*` timeline types. Without this arm the
///   operator sees nothing after `/curate` — the curator's child session is
///   slash-form now, so root attribution works, but workflow events only
///   reach the timeline via this mirror.
///
/// The two paths never overlap: a task id is either `curate-...` (enqueued by
/// the operator RPC into `wf-{root}`) or it's in a `sched-{job_id}` workflow.
fn maybe_emit_workflow_timeline(
    config: &GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
    event: &WorkflowEventRecord,
) {
    let is_sched = event.workflow_id.starts_with("sched-");
    // `/curate` task ids are minted at router.rs via
    // `id_format::short_random_id("curate-")` → `curate-{8-hex}`. This prefix
    // is stable and unique to the operator curation RPC — nothing else mints
    // `curate-` task ids (the cron path uses `sched-{job_id}` workflows with
    // `task-{...}` ids via `new_task_id()`).
    let is_curation = event
        .task_id
        .as_deref()
        .is_some_and(|t| t.starts_with("curate-"));
    if !is_sched && !is_curation {
        return;
    }
    let root_session_id = store
        .resolve_root_session_id(&event.workflow_id)
        .ok()
        .flatten()
        .or_else(|| {
            load_workflow_run(config, Some(store), &event.workflow_id)
                .ok()
                .flatten()
                .map(|r| r.root_session_id)
        })
        .filter(|s| !s.is_empty());
    let Some(root_session_id) = root_session_id else {
        return;
    };
    let agent_label = workflow_agent_label(event.agent_id.as_deref());

    // Manual `/curate` arm — emit `curation.completed` / `curation.failed`
    // only on terminal task transitions. The spawn is already surfaced by the
    // `workflow.task.spawned` causal event emitted in the /curate handler; we
    // only need to mirror the result so the operator sees when the curator
    // finishes and what it concluded.
    if is_curation {
        let (timeline_type, payload) = match event.event_type.as_str() {
            "task.completed" => {
                let summary = event
                    .payload
                    .get("result_summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (
                    "curation.completed",
                    serde_json::json!({
                        "agent_id": agent_label,
                        "task_id": event.task_id,
                        "result_summary": summary,
                    }),
                )
            }
            "task.failed" => {
                let summary = event
                    .payload
                    .get("result_summary")
                    .or_else(|| event.payload.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("curator task failed");
                (
                    "curation.failed",
                    serde_json::json!({
                        "agent_id": agent_label,
                        "task_id": event.task_id,
                        "result_summary": summary,
                    }),
                )
            }
            _ => return,
        };
        emit_timeline_event(store, &root_session_id, &agent_label, timeline_type, payload);
        return;
    }

    // Scheduled-job arm (unchanged behavior).
    let (timeline_type, payload) = match event.event_type.as_str() {
        "scheduled_job.triggered" => (
            "scheduled_job.triggered",
            serde_json::json!({
                "agent_id": agent_label,
                "job_id": event.payload.get("job_id").and_then(|v| v.as_str()),
                "task_id": event.task_id,
            }),
        ),
        "task.completed" => {
            let summary = event
                .payload
                .get("result_summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            (
                "scheduled_job.completed",
                serde_json::json!({
                    "agent_id": agent_label,
                    "task_id": event.task_id,
                    "result_summary": summary,
                }),
            )
        }
        "task.failed" => {
            let summary = event
                .payload
                .get("result_summary")
                .or_else(|| event.payload.get("reason"))
                .and_then(|v| v.as_str())
                .unwrap_or("task failed");
            (
                "scheduled_job.failed",
                serde_json::json!({
                    "agent_id": agent_label,
                    "task_id": event.task_id,
                    "result_summary": summary,
                }),
            )
        }
        _ => return,
    };
    emit_timeline_event(store, &root_session_id, &agent_label, timeline_type, payload);
}

/// Build and persist a `live_digest_events` row. Shared by both arms of
/// `maybe_emit_workflow_timeline` so the timeline-event shape stays identical
/// for scheduled-job and curation events.
fn emit_timeline_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    root_session_id: &str,
    agent_label: &str,
    timeline_type: &str,
    payload: serde_json::Value,
) {
    let principal = autonoetic_types::principal::Principal::agent(agent_label);
    let role = crate::runtime::session_timeline::derive_role(agent_label);
    let tl_event = crate::runtime::session_timeline::build_timeline_event(
        root_session_id.to_string(),
        root_session_id.to_string(),
        None,
        &principal,
        &role,
        timeline_type,
        None,
        Some(payload),
        autonoetic_types::session_timeline::TimelineRefs::default(),
    );
    if let Err(e) = store.create_live_digest_event(&tl_event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            event_type = timeline_type,
            "workflow timeline emit failed"
        );
    }
}

fn workflow_agent_label(agent_id: Option<&str>) -> String {
    agent_id
        .unwrap_or("agent")
        .split('@')
        .next()
        .unwrap_or("agent")
        .to_string()
}

/// Append a primary-workflow event when a cron job is cancelled so the chat TUI can show it.
///
/// Uses the root session's indexed primary workflow (`workflow_index` / file fallback). If no
/// workflow is registered yet, logs at debug and returns Ok.
pub fn append_scheduled_job_cancelled_workflow_event(
    config: &GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
    root_session_id: &str,
    job_id: &str,
    owner_agent_id: &str,
    target_agent_id: &str,
    cron_expr: &str,
    cancel_reason: &str,
) -> anyhow::Result<()> {
    let Some(primary_wf) = resolve_workflow_id_for_root_session(config, root_session_id)? else {
        tracing::debug!(
            target: "scheduler",
            root_session_id = %root_session_id,
            job_id = %job_id,
            "No primary workflow index for root session; skipping scheduled_job.cancelled event"
        );
        return Ok(());
    };
    let occurred_at = chrono::Utc::now().to_rfc3339();
    let suffix = autonoetic_types::id_format::short_random_id("");
    let event = autonoetic_types::workflow::WorkflowEventRecord {
        event_id: format!("wevt-sjcancel-{job_id}-{suffix}"),
        workflow_id: primary_wf,
        task_id: None,
        event_type: "scheduled_job.cancelled".to_string(),
        agent_id: Some(target_agent_id.to_string()),
        payload: serde_json::json!({
            "job_id": job_id,
            "owner_agent_id": owner_agent_id,
            "target_agent_id": target_agent_id,
            "cron_expr": cron_expr,
            "root_session_id": root_session_id,
            "cancel_reason": cancel_reason,
        }),
        occurred_at,
    };
    append_workflow_event(config, Some(store), &event)
}

/// Write or replace a task record and refresh `workflow.active_task_ids`.
///
/// `active_task_ids` is a denormalized "still in flight" list: only
/// non-terminal-for-join tasks belong in it. Terminal tasks must not be
/// (re-)added — otherwise failed/succeeded tasks linger as phantom "active"
/// entries that block workflow completion (`try_complete_workflow`) and,
/// historically, over-triggered the child-wait park predicate. Removal of
/// the saved task on terminal saves, and repair of pre-existing drift, is
/// handled here and by the scheduler janitor
/// (`reconcile_paused_child_wait_tasks`).
pub fn save_task_run(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    task: &TaskRun,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = Some(GatewayStore::open(&gateway_dir)?);
            owned_store.as_ref().unwrap()
        }
    };
    store.upsert_task_run(task)?;

    let mut run = load_workflow_run(config, Some(store), &task.workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", task.workflow_id))?;
    if task.status.is_terminal_for_join() {
        run.active_task_ids.retain(|id| id != &task.task_id);
    } else if !run.active_task_ids.contains(&task.task_id) {
        run.active_task_ids.push(task.task_id.clone());
    }
    run.updated_at = now_rfc3339();
    save_workflow_run(config, Some(store), &run)?;
    Ok(())
}

/// Update a task run's metadata.
pub fn update_task_run_metadata(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    metadata: serde_json::Value,
) -> anyhow::Result<()> {
    let mut task = load_task_run(config, store, workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task '{}' not in workflow '{}'", task_id, workflow_id))?;
    task.metadata = Some(metadata);
    task.updated_at = now_rfc3339();
    save_task_run(config, store, &task)
}

/// Load a task run. SQLite is the source of truth; legacy files are read once
/// and migrated into SQLite on first touch.
pub fn load_task_run(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskRun>> {
    let mut owned_store = None;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = Some(GatewayStore::open(&gateway_dir)?);
            owned_store.as_ref().unwrap()
        }
    };

    if let Some(task) = store.get_task_run(workflow_id, task_id)? {
        return Ok(Some(task));
    }

    let p = task_run_path(config, workflow_id, task_id);
    if !p.exists() {
        return Ok(None);
    }
    let task: TaskRun = read_json_file(&p)?;
    store.upsert_task_run(&task)?;
    let _ = fs::remove_file(&p);
    Ok(Some(task))
}

/// List all persisted [`TaskRun`] records for a workflow. Legacy files are
/// migrated into SQLite on the first scan.
pub fn list_task_runs_for_workflow(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<TaskRun>> {
    let mut owned_store = None;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = Some(GatewayStore::open(&gateway_dir)?);
            owned_store.as_ref().unwrap()
        }
    };

    migrate_legacy_task_files(config, store, workflow_id)?;

    store.list_task_runs_for_workflow(workflow_id)
}

/// One-time migration of legacy task JSON files into SQLite. Files that are
/// successfully migrated are deleted so SQLite becomes the single source of truth.
fn migrate_legacy_task_files(
    config: &GatewayConfig,
    store: &GatewayStore,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let dir: PathBuf = workflow_run_dir(config, workflow_id).join("tasks");
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_json_file::<TaskRun>(&path) {
            Ok(t) => match store.upsert_task_run(&t) {
                Ok(()) => {
                    let _ = fs::remove_file(&path);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to migrate legacy task json into SQLite; leaving file in place",
                    );
                }
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip invalid legacy task json");
            }
        }
    }
    Ok(())
}

/// Active workflow approval gate: any task in `AwaitingApproval` or workflow `BlockedApproval`.
#[derive(Debug, Clone)]
pub struct WorkflowApprovalGate {
    pub workflow_id: String,
    pub awaiting_task_ids: Vec<String>,
    pub pending_approval_request_ids: Vec<String>,
}

pub fn workflow_approval_gate_active(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowApprovalGate>> {
    let Some(wf) = load_workflow_run(config, store, workflow_id)? else {
        return Ok(None);
    };
    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    let awaiting_task_ids: Vec<String> = tasks
        .iter()
        .filter(|task| task.status == TaskRunStatus::AwaitingApproval)
        .map(|task| task.task_id.clone())
        .collect();
    let gate_active =
        wf.status == WorkflowRunStatus::BlockedApproval || !awaiting_task_ids.is_empty();
    if !gate_active {
        return Ok(None);
    }

    let mut pending_approval_request_ids = Vec::new();
    if let Some(store) = store {
        for task_id in &awaiting_task_ids {
            if let Ok(Some(request_id)) = store.get_pending_approval_request_id_for_task(task_id) {
                pending_approval_request_ids.push(request_id);
            }
        }
    }

    Ok(Some(WorkflowApprovalGate {
        workflow_id: workflow_id.to_string(),
        awaiting_task_ids,
        pending_approval_request_ids,
    }))
}

/// Keep `WorkflowRunStatus::BlockedApproval` aligned with task-level `AwaitingApproval` rows.
pub fn sync_workflow_blocked_approval_status(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let Some(mut wf) = load_workflow_run(config, store, workflow_id)? else {
        return Ok(());
    };
    if matches!(
        wf.status,
        WorkflowRunStatus::EmergencyStopping
            | WorkflowRunStatus::EmergencyStopped
            | WorkflowRunStatus::Completed
            | WorkflowRunStatus::Failed
            | WorkflowRunStatus::Cancelled
    ) {
        return Ok(());
    }

    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    let any_awaiting = tasks
        .iter()
        .any(|task| task.status == TaskRunStatus::AwaitingApproval);
    if any_awaiting {
        if wf.status != WorkflowRunStatus::BlockedApproval {
            wf.status = WorkflowRunStatus::BlockedApproval;
            wf.updated_at = now_rfc3339();
            save_workflow_run(config, store, &wf)?;
        }
    } else if wf.status == WorkflowRunStatus::BlockedApproval {
        wf.status = WorkflowRunStatus::WaitingChildren;
        wf.updated_at = now_rfc3339();
        save_workflow_run(config, store, &wf)?;
    }
    Ok(())
}

/// Update task status (and optional result summary) and append a completion-style event.
pub fn update_task_run_status(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    status: TaskRunStatus,
    result_summary: Option<String>,
    approval_metadata: Option<ApprovalMetadata>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) -> anyhow::Result<()> {
    let mut task = load_task_run(config, store, workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task '{}' not in workflow '{}'", task_id, workflow_id))?;

    let previous_status = task.status;

    // Issue #740: enforce transition legality at the single mutation choke
    // point. If the transition is illegal (e.g. Succeeded -> Runnable), log
    // with full context and refuse the mutation (return Ok-noop). This lets
    // us delete the scattered ad-hoc guards at call sites — the store now
    // refuses nonsense for everyone, including future callers.
    if !previous_status.try_transition(status) {
        tracing::warn!(
            target: "workflow",
            workflow_id = %workflow_id,
            task_id = %task_id,
            previous_status = %previous_status.as_str(),
            attempted_status = %status.as_str(),
            "Illegal task status transition refused; leaving task in its current state"
        );
        return Ok(());
    }

    // Store previous status for implicit artifact creation
    let was_succeeded = task.status == TaskRunStatus::Succeeded;
    let is_now_succeeded = status == TaskRunStatus::Succeeded;

    let retry_decision = evaluate_stage_retry(&task, status, result_summary.as_deref());
    let task_failure = retry_decision.failure.clone();
    let child_state_notification = build_child_state_notification(
        &task,
        status,
        result_summary.as_deref(),
        task_failure.as_ref(),
    );

    task.status = status;
    task.updated_at = now_rfc3339();
    task.result_summary = result_summary.clone();
    if let Some(ref failure) = task_failure {
        task.last_failure_class = failure.failure_class;
        task.side_effect_state = failure.side_effect_state;
    }
    save_task_run(config, store, &task)?;

    // Session-workflow sync (#673 GAP-2A): close out the bound session's
    // transcript BEFORE emitting any signals/events so that a woken parent
    // querying child state sees the correct terminal status, not stale
    // 'active'/'suspended'. In the normal spawn-completion path the
    // executor's close_session has already recorded a status (this overwrites
    // it harmlessly). In external paths (stuck sweeper, approval timeout,
    // cancel, force-complete) this is the ONLY place the session is closed out
    // at all.
    //
    // It *terminates* rather than politely finalizes. A session bound to a task
    // that just reached a terminal status can never run again — a retried stage
    // gets a fresh task and a fresh session id — whereas
    // `finalize_session_transcript` preserves resumable states
    // (`hibernated`/`awaiting_gate`/`idle`/`paused`) and would leave a dead
    // child advertising a resumable lifecycle. That lie is invisible while the
    // parent is alive and becomes an endless reap loop the moment the parent
    // ends.
    {
        let is_terminal = status.is_terminal();
        let was_non_terminal = !previous_status.is_terminal();
        if is_terminal && was_non_terminal && !task.session_id.is_empty() {
            if let Some(store) = store {
                let session_status = match status {
                    TaskRunStatus::Succeeded => "completed",
                    _ => "failed",
                };
                let ended_at = now_rfc3339();
                if let Err(e) =
                    store.terminate_session_transcript(&task.session_id, &ended_at, session_status)
                {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        session_id = %task.session_id,
                        error = %e,
                        "Failed to finalize session transcript on terminal task transition"
                    );
                }
            }
        }

        // Release any singleton slot held by this task so the same singleton
        // can be reacquired in the workflow.
        if is_terminal && was_non_terminal {
            if let Some(store) = store {
                if let Err(e) = store.release_singleton_slot_by_task_id(workflow_id, task_id) {
                    tracing::warn!(
                        target: "singleton_dedup",
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        error = %e,
                        "Failed to release singleton slot on terminal task transition"
                    );
                }
            }
        }
    }

    if status == TaskRunStatus::Cancelled && previous_status == TaskRunStatus::AwaitingApproval {
        let cancel_reason = result_summary
            .as_deref()
            .unwrap_or("workflow_task_cancelled");
        let _ = crate::scheduler::approval::cancel_pending_approval_for_workflow_task(
            config,
            store,
            task_id,
            "gateway",
            cancel_reason,
        );
    }
    if previous_status == TaskRunStatus::AwaitingApproval
        || status == TaskRunStatus::AwaitingApproval
    {
        let _ = sync_workflow_blocked_approval_status(config, store, workflow_id);
    }

    // Create implicit artifact when task succeeds (transition to Succeeded)
    if is_now_succeeded && !was_succeeded {
        if let Err(e) = create_implicit_artifact(config, store, &task, result_summary.as_deref()) {
            tracing::warn!(
                target: "workflow",
                task_id = %task_id,
                error = %e,
                "Failed to create implicit artifact"
            );
        }
    }

    let event_type = match &status {
        TaskRunStatus::Succeeded => "task.completed",
        TaskRunStatus::Failed | TaskRunStatus::Cancelled | TaskRunStatus::Aborted => "task.failed",
        TaskRunStatus::AwaitingApproval => "task.awaiting_approval",
        TaskRunStatus::Running => "task.started",
        _ => "task.updated",
    };

    // Build event payload, including approval metadata for AwaitingApproval status
    let payload = if matches!(status, TaskRunStatus::AwaitingApproval) {
        let mut payload = if let Some(meta) = approval_metadata {
            serde_json::json!({
                "status": status,
                "approval_request_id": meta.request_id,
                "approval": meta.kind,
                "reason": meta.reason,
            })
        } else {
            serde_json::json!({ "status": status })
        };
        if let Some(ref failure) = task_failure {
            if let Some(obj) = payload.as_object_mut() {
                failure.apply_to_json_map(obj);
            }
        }
        payload
    } else if matches!(status, TaskRunStatus::Succeeded) {
        let agent_outcome = result_summary
            .as_deref()
            .and_then(autonoetic_types::task_completion::extract_agent_outcome)
            .map(|o| o.as_str());
        let mut payload = serde_json::json!({
            "status": status,
            "result_summary": result_summary,
        });
        if let Some(outcome) = agent_outcome {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "agent_outcome".to_string(),
                    serde_json::Value::String(outcome.to_string()),
                );
            }
        }
        payload
    } else {
        let mut payload = serde_json::json!({ "status": status });
        if let Some(ref summary) = result_summary {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(
                    "result_summary".to_string(),
                    serde_json::Value::String(summary.clone()),
                );
            }
        }
        if let Some(ref failure) = task_failure {
            if let Some(obj) = payload.as_object_mut() {
                failure.apply_to_json_map(obj);
            }
        }
        payload
    };

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: Some(task_id.to_string()),
            event_type: event_type.to_string(),
            agent_id: Some(task.agent_id.clone()),
            payload,
            occurred_at: now_rfc3339(),
        },
    )?;

    if retry_decision.budget_exhausted {
        append_workflow_event(
            config,
            store,
            &WorkflowEventRecord {
                event_id: new_event_id(),
                workflow_id: workflow_id.to_string(),
                task_id: Some(task_id.to_string()),
                event_type: "workflow.stage_budget_exhausted".to_string(),
                agent_id: Some(task.agent_id.clone()),
                payload: serde_json::json!({
                    "task_id": task_id,
                    "failure_class": task_failure.as_ref().and_then(|failure| failure.failure_class).and_then(|value| serde_json::to_value(value).ok()),
                    "retry_advice": task_failure.as_ref().and_then(|failure| failure.retry_advice).and_then(|value| serde_json::to_value(value).ok()),
                    "retry_count": task.retry_count,
                    "max_retries": retry_decision.max_retries,
                }),
                occurred_at: now_rfc3339(),
            },
        )?;
    }

    if matches!(
        status,
        TaskRunStatus::Succeeded
            | TaskRunStatus::Failed
            | TaskRunStatus::Cancelled
            | TaskRunStatus::Aborted
    ) {
        if let Some(dedupe_key) = task.dedupe_key.as_deref() {
            if let Err(error) = crate::scheduler::single_flight::release_reservation(
                config,
                workflow_id,
                dedupe_key,
            ) {
                tracing::warn!(
                    target: "workflow",
                    workflow_id = %workflow_id,
                    task_id = %task_id,
                    error = %error,
                    "Failed to release single-flight reservation"
                );
            }
        }
    }

    if let Some(child_event_type) = child_state_event_type(status) {
        append_workflow_event(
            config,
            store,
            &WorkflowEventRecord {
                event_id: new_event_id(),
                workflow_id: workflow_id.to_string(),
                task_id: Some(task_id.to_string()),
                event_type: child_event_type.to_string(),
                agent_id: Some(task.agent_id.clone()),
                payload: serde_json::to_value(&child_state_notification)?,
                occurred_at: now_rfc3339(),
            },
        )?;
    }

    if let Some(wf) = load_workflow_run(config, store, workflow_id)? {
        let root_session_id = wf.root_session_id.clone();
        let (causal_action, causal_status) = match status {
            TaskRunStatus::Succeeded => ("workflow.task.completed", EntryStatus::Success),
            TaskRunStatus::Failed | TaskRunStatus::Cancelled | TaskRunStatus::Aborted => {
                ("workflow.task.failed", EntryStatus::Error)
            }
            TaskRunStatus::AwaitingApproval => {
                ("workflow.task.awaiting_approval", EntryStatus::Success)
            }
            _ => ("workflow.task.updated", EntryStatus::Success),
        };
        crate::scheduler::workflow_causal::mirror_orchestration_event(
            config,
            &root_session_id,
            causal_action,
            causal_status,
            serde_json::json!({
                "workflow_id": workflow_id,
                "task_id": task_id,
                "workflow_event_type": event_type,
                "target_agent_id": task.agent_id,
                "child_session_id": task.session_id,
                "parent_session_id": task.parent_session_id,
            }),
        );

        // Set workflow to BlockedApproval when a task enters AwaitingApproval
        if status == TaskRunStatus::AwaitingApproval
            && wf.status != WorkflowRunStatus::BlockedApproval
        {
            let mut wf_update = wf.clone();
            wf_update.status = WorkflowRunStatus::BlockedApproval;
            wf_update.updated_at = now_rfc3339();
            if let Err(e) = save_workflow_run(config, store, &wf_update) {
                tracing::warn!(target: "workflow", error = %e, "Failed to set BlockedApproval");
            }
        }

        // Check join condition after terminal task updates.
        // `Stale` counts as terminal-for-join (unblocks joins) but not as a
        // session-terminal status (see #722 Stage 2 / P-2.11).
        let is_terminal = status.is_terminal_for_join();
        let wf_not_emergency_stopped = !matches!(
            wf.status,
            WorkflowRunStatus::EmergencyStopping | WorkflowRunStatus::EmergencyStopped
        );
        let mut join_just_satisfied = false;
        // Bug fix: removed !wf.join_task_ids.is_empty() check - check_join_condition correctly
        // returns true for empty join_task_ids, and workflows with empty join_task_ids need to
        // transition to Resumable when tasks complete
        if is_terminal && wf_not_emergency_stopped && wf.status != WorkflowRunStatus::Resumable {
            if let Ok(true) = check_join_condition(config, store, workflow_id) {
                join_just_satisfied = true;
                let mut wf_mut = wf;
                wf_mut.status = WorkflowRunStatus::Resumable;
                wf_mut.updated_at = now_rfc3339();
                if let Err(e) = save_workflow_run(config, store, &wf_mut) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        "Failed to mark workflow as Resumable"
                    );
                }
                append_workflow_event(
                    config,
                    store,
                    &WorkflowEventRecord {
                        event_id: new_event_id(),
                        workflow_id: workflow_id.to_string(),
                        task_id: None,
                        event_type: "workflow.join.satisfied".to_string(),
                        agent_id: None,
                        payload: serde_json::json!({
                            "join_task_ids": wf_mut.join_task_ids,
                        }),
                        occurred_at: now_rfc3339(),
                    },
                )?;
                tracing::info!(
                    target: "workflow",
                    workflow_id = %workflow_id,
                    "Join condition satisfied — workflow marked Resumable"
                );

                // Gather child summaries for all terminal join tasks so the
                // planner doesn't need separate workflow_state + artifact
                // inspect rounds to see what each child produced.
                let child_summaries =
                    gather_join_child_summaries(config, store, workflow_id, &wf_mut.join_task_ids);

                let hook_delivers_join_signal = hook_executor.is_some_and(|executor| {
                    executor.has_deliver_signal_hook(
                        autonoetic_types::hooks::HookEvent::WorkflowJoinSatisfied,
                    )
                });

                // Send a signal to the planner session to resume unless a workflow.join.satisfied
                // deliver_signal hook is already configured to do so.
                if !hook_delivers_join_signal {
                    if let Err(e) = crate::scheduler::signal::send_workflow_join_satisfied(
                        store,
                        &wf_mut.root_session_id,
                        workflow_id,
                        wf_mut.join_task_ids.clone(),
                        child_summaries,
                    ) {
                        tracing::warn!(
                            target: "signal",
                            error = %e,
                            "Failed to send workflow join satisfied signal"
                        );
                    }
                }

                if let Some(executor) = hook_executor {
                    let ctx = autonoetic_types::hooks::HookContext::for_workflow_join_satisfied(
                        &wf_mut.root_session_id,
                        workflow_id,
                        &wf_mut.join_task_ids,
                    );
                    executor.dispatch_async(ctx);
                }
            }
        }

        // Send per-child notification — but skip it when the join was just
        // satisfied in this same update AND the target is the root session.
        // The join-satisfied signal already carries all child summaries and is
        // sent to the root, so sending both to the root would be redundant and
        // cause a double planner wake. For nested workflows, the immediate
        // parent session differs from the root and still needs this wake to
        // resume its own workflow_wait.
        if let Some(child_event_type) = child_state_event_type(status) {
            let target_session_id = if task.parent_session_id.is_empty() {
                root_session_id.as_str()
            } else {
                task.parent_session_id.as_str()
            };
            if !join_just_satisfied || target_session_id != root_session_id.as_str() {
                tracing::info!(
                    target: "workflow",
                    workflow_id = %workflow_id,
                    task_id = %task_id,
                    event_type = %child_event_type,
                    "Emitting child-state notification to parent session"
                );
                if let Err(e) = crate::scheduler::signal::send_child_state_notification(
                    store,
                    target_session_id,
                    child_state_notification,
                ) {
                    tracing::warn!(
                        target: "signal",
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        error = %e,
                        "Failed to send child-state notification"
                    );
                }
            }
        }

        // #845 follow-up: when a task reaches a terminal-for-join state (this
        // includes `Stale`, so approval-timed-out children also unblock
        // parked tasks), re-evaluate every Paused child-wait task in the
        // workflow and wake those whose wait set is now empty. Without this
        // transition, Fix 2's `Paused` state has no return path — the task
        // would sit Paused forever and the workflow would deadlock (the join
        // blocks on the non-terminal Paused task).
        // `process_queued_workflow_tasks` skips Paused tasks
        // (scheduler.rs:1532), and `process_pending_notifications` delivers
        // the ChildStateNotification via event.ingest which the router
        // reroutes to the *root* planner (router.rs:1093), so neither path
        // wakes the parked task. The scan is workflow-scoped and
        // condition-based: it wakes any parked task whose own wait set is
        // empty, not just the terminating task's direct parent — a task
        // parked because of a *sibling* must also be woken (session-d484ea13
        // deadlock). We do it here, atomically with the terminal transition.
        if status.is_terminal_for_join() {
            if let Err(e) = wake_paused_child_wait_tasks(
                config,
                store,
                workflow_id,
                task_id,
                &task.session_id,
            ) {
                tracing::warn!(
                    target: "workflow",
                    workflow_id = %workflow_id,
                    task_id = %task_id,
                    error = %e,
                    "Failed to wake Paused child-wait tasks after terminal transition"
                );
            }
        }
    }

    Ok(())
}

/// Shared park/wake predicate: does this session have any non-terminal
/// workflow tasks that it spawned (i.e. rows whose `parent_session_id` is this
/// session)? Rows with an empty `parent_session_id` are attributed to the root
/// session, matching the notification fallback in `update_task_run_status`.
///
/// This is deliberately parent-scoped and status-authoritative:
/// - it does NOT consult `WorkflowRun.active_task_ids`/`queued_task_ids`
///   (denormalized lists that can drift — `save_task_run` appends on every
///   save while `dequeue_task` is the only remover);
/// - it does NOT count siblings or cousins in the same workflow. A leaf task
///   that finishes while a sibling is still running has no wait set and must
///   complete normally, not park (the session-d484ea13 deadlock).
///
/// The SAME predicate backs all three sides of the suspend/resume machinery so
/// they can never disagree again: the EndTurn/MaxTokens park predicate
/// (`lifecycle::waiting_for_child_yield_reason`), the child-wait wake scan
/// (`wake_paused_child_wait_tasks`), and the scheduler janitor
/// (`reconcile_paused_child_wait_tasks`).
pub(crate) fn session_has_non_terminal_children(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    session_id: &str,
) -> anyhow::Result<bool> {
    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    let root_session_id = crate::runtime::content_store::root_session_id(session_id);
    Ok(tasks.iter().any(|t| {
        (t.parent_session_id == session_id
            || (t.parent_session_id.is_empty() && session_id == root_session_id))
            && !t.status.is_terminal_for_join()
    }))
}

/// #845 follow-up — wake `Paused` child-wait tasks whose wait set is now empty,
/// so the scheduler can pick them up and resume their sessions.
///
/// Fix 2 (`run_durable_workflow_task::suspended_for_child_wait` branch) marks
/// a task `Paused` when its session ends its turn with pending async children.
/// But nothing else transitions `Paused` back to `Runnable` on child
/// completion:
/// - `process_queued_workflow_tasks` (scheduler.rs:1532) explicitly skips
///   `Paused` tasks and dequeues them;
/// - `process_pending_notifications` delivers the ChildStateNotification via
///   `event.ingest`, which the router reroutes to the *root* planner session
///   (router.rs:1093) — the intermediate parent never receives it;
/// - `TaskNotifyRegistry::notify_session` is fired but never awaited in
///   production code.
///
/// This helper closes the gap. It is invoked from `update_task_run_status`
/// right after the child-state notification is emitted, so the wake happens
/// atomically with the child's terminal transition, and from the scheduler
/// janitor (`reconcile_paused_child_wait_tasks`) as a safety net for any
/// missed transition.
///
/// The scan is **workflow-scoped and condition-based** (sibling-deadlock fix):
/// rather than waking only the terminating task's direct parent (which
/// stranded tasks parked because of a *sibling*), every `Paused` task parked
/// in `"paused_child_wait"` re-evaluates its own wait set via
/// `session_has_non_terminal_children` — the same predicate that parked it —
/// and wakes only when that set is empty. A parked task whose children are
/// still running stays parked, no matter which sibling resolved.
///
/// To avoid masking `Paused`-for-user-input tasks (which have their own
/// wake-up via `interaction_answer.rs:343`), the helper inspects the task's
/// checkpoint `step` field and only wakes tasks parked in
/// `"paused_child_wait"` (the label Fix 2 writes). A `Paused` task without a
/// checkpoint, or with any other step, is left untouched.
pub(crate) fn wake_paused_child_wait_tasks(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    triggered_by_task_id: &str,
    triggered_by_session_id: &str,
) -> anyhow::Result<()> {
    let Some(store) = store else {
        return Ok(());
    };
    let tasks = list_task_runs_for_workflow(config, Some(store), workflow_id)?;
    for mut task in tasks {
        if task.status != TaskRunStatus::Paused {
            continue;
        }

        // Only wake tasks that are explicitly parked for child-wait. Other
        // Paused tasks (e.g. user_input via run_durable_workflow_task's
        // earlier branch) have their own wake-up paths and must not be
        // disturbed by an unrelated child's terminal transition.
        let is_child_wait = match load_task_checkpoint(
            config,
            Some(store),
            workflow_id,
            &task.task_id,
        )? {
            Some(cp) => cp.step == "paused_child_wait",
            None => false,
        };
        if !is_child_wait {
            continue;
        }

        // Condition-based wake: only wake when the parked session's own wait
        // set (non-terminal children it spawned) is empty. The resumed turn
        // re-evaluates the same predicate at EndTurn, so a spurious wake is
        // harmless — but skipping one is a deadlock, so we never gate on
        // which task triggered this scan.
        if session_has_non_terminal_children(config, Some(store), workflow_id, &task.session_id)? {
            continue;
        }

        // Transition Paused → Runnable directly via `save_task_run` rather
        // than recursing through `update_task_run_status`: a Runnable
        // transition would otherwise emit another "workflow.child.resolved"
        // notification to the *grandparent*, misleading it into thinking
        // the parent has finished. We are merely unpausing, not resolving.
        task.status = TaskRunStatus::Runnable;
        task.updated_at = now_rfc3339();
        save_task_run(config, Some(store), &task)?;

        append_workflow_event(
            config,
            Some(store),
            &WorkflowEventRecord {
                event_id: new_event_id(),
                workflow_id: workflow_id.to_string(),
                task_id: Some(task.task_id.clone()),
                event_type: "task.updated".to_string(),
                agent_id: Some(task.agent_id.clone()),
                payload: serde_json::json!({
                    "status": "runnable",
                    "reason": "child_wait_set_empty",
                    "triggered_by_task_id": triggered_by_task_id,
                    "triggered_by_child_session": triggered_by_session_id,
                }),
                occurred_at: now_rfc3339(),
            },
        )?;

        tracing::info!(
            target: "workflow",
            workflow_id = %workflow_id,
            woken_task_id = %task.task_id,
            woken_session_id = %task.session_id,
            triggered_by_task_id = %triggered_by_task_id,
            "Woke Paused child-wait task: wait set empty, re-queueing for execution (Ri-0.14 / #845 follow-up)"
        );
    }
    Ok(())
}

/// Safety-net janitor for the workflow suspend/resume machinery, run every
/// scheduler tick via `scheduler::reconcile_paused_child_wait_tasks`.
///
/// `Paused` tasks parked in `paused_child_wait` are normally woken by
/// `wake_paused_child_wait_tasks` atomically with a child's terminal
/// transition — but any missed transition (crash between the status save and
/// the wake, state written by older builds, a sibling-parked task from the
/// pre-fix parent-only wake) used to strand the task — and the workflow join
/// — forever, because `process_queued_workflow_tasks` skips Paused tasks and
/// no sweeper covered them.
///
/// For every workflow run:
/// 1. Repair drifted `active_task_ids` (drop terminal-for-join entries that
///    were never dequeued, re-add live ones) so completion logic
///    (`try_complete_workflow`) and operators see the truth.
/// 2. If the workflow run itself is terminal, fail any task still parked in
///    `paused_child_wait` — its wait can never be answered.
/// 3. Otherwise re-run the condition-based wake scan: any parked task whose
///    own wait set (non-terminal children it spawned, per
///    `session_has_non_terminal_children`) is empty is marked `Runnable`.
pub(crate) fn reconcile_paused_child_wait_tasks(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
) -> anyhow::Result<()> {
    let workflows_root = workflows_root(config).join("runs");
    if !workflows_root.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&workflows_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let wf_id = entry.file_name().to_string_lossy().to_string();
        let Some(run) = load_workflow_run(config, store, &wf_id)? else {
            continue;
        };
        let tasks = list_task_runs_for_workflow(config, store, &wf_id)?;

        // 1. Repair active_task_ids drift.
        {
            use std::collections::HashSet;
            let live: HashSet<&str> = tasks
                .iter()
                .filter(|t| !t.status.is_terminal_for_join())
                .map(|t| t.task_id.as_str())
                .collect();
            let mut repaired: Vec<String> = run
                .active_task_ids
                .iter()
                .filter(|id| live.contains(id.as_str()))
                .cloned()
                .collect();
            for t in &tasks {
                if live.contains(t.task_id.as_str()) && !repaired.contains(&t.task_id) {
                    repaired.push(t.task_id.clone());
                }
            }
            if repaired != run.active_task_ids {
                tracing::info!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    before = ?run.active_task_ids,
                    after = ?repaired,
                    "Repaired drifted workflow active_task_ids"
                );
                let mut run_mut = run.clone();
                run_mut.active_task_ids = repaired;
                run_mut.updated_at = now_rfc3339();
                save_workflow_run(config, store, &run_mut)?;
            }
        }

        // 2. Terminal workflow: fail orphaned parked child-wait tasks.
        if run.status.is_terminal() {
            for task in &tasks {
                if task.status != TaskRunStatus::Paused {
                    continue;
                }
                let is_child_wait = load_task_checkpoint(config, store, &wf_id, &task.task_id)
                    .ok()
                    .flatten()
                    .map(|cp| cp.step == "paused_child_wait")
                    .unwrap_or(false);
                if !is_child_wait {
                    continue;
                }
                tracing::info!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    task_id = %task.task_id,
                    "Failing orphaned child-wait task: workflow is terminal"
                );
                if let Err(e) = update_task_run_status(
                    config,
                    store,
                    &wf_id,
                    &task.task_id,
                    TaskRunStatus::Failed,
                    Some("workflow terminated while parked in child-wait".to_string()),
                    None,
                    None,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        task_id = %task.task_id,
                        error = %e,
                        "Failed to fail orphaned child-wait task"
                    );
                }
            }
            continue;
        }

        // 3. Condition-based wake scan (safety net for missed transitions).
        wake_paused_child_wait_tasks(config, store, &wf_id, "janitor", "scheduler")?;
    }

    Ok(())
}

pub(crate) fn schedule_task_stage_retry(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    result_summary: Option<String>,
    decision: &StageRetryDecision,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        decision.retry_scheduled,
        "stage retry is not scheduled for this task"
    );

    let mut task = load_task_run(config, store, workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task '{}' not in workflow '{}'", task_id, workflow_id))?;
    task.status = TaskRunStatus::Runnable;
    task.updated_at = now_rfc3339();
    task.result_summary = result_summary.clone();
    task.retry_count = decision.next_retry_count;
    if let Some(ref failure) = decision.failure {
        task.last_failure_class = failure.failure_class;
        task.side_effect_state = failure.side_effect_state;
    }
    save_task_run(config, store, &task)?;

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: Some(task_id.to_string()),
            event_type: "task.updated".to_string(),
            agent_id: Some(task.agent_id.clone()),
            payload: serde_json::json!({
                "status": TaskRunStatus::Runnable,
                "result_summary": result_summary,
                "retry_count": task.retry_count,
                "failure_class": decision.failure.as_ref().and_then(|failure| failure.failure_class).and_then(|value| serde_json::to_value(value).ok()),
                "retry_advice": decision.failure.as_ref().and_then(|failure| failure.retry_advice).and_then(|value| serde_json::to_value(value).ok()),
                "side_effect_state": decision.failure.as_ref().and_then(|failure| failure.side_effect_state).and_then(|value| serde_json::to_value(value).ok()),
            }),
            occurred_at: now_rfc3339(),
        },
    )?;

    Ok(())
}

/// Operator-initiated retry of a terminal task.
///
/// Moves a `Failed` (or `Aborted`) task back to `Runnable` so the durable
/// scheduler re-queues and re-spawns it. This is the manual escape hatch for a
/// task that exhausted its automatic stage-retry budget (or never had one) —
/// e.g. a child that crashed on a transient LLM error past the driver retries.
///
/// Unlike [`schedule_task_stage_retry`], this:
/// - is an explicit operator action, not an automated state-machine transition;
/// - deliberately bypasses [`TaskRunStatus::try_transition`], which forbids
///   `Failed -> Runnable` (that guard is for *automated* correctness; an
///   operator override is exactly the escape hatch we want here);
/// - reactivates the parent [`WorkflowRun`] if it is itself terminal, mirroring
///   the root-spawn reactivation in `agent.rs` (status -> `Resumable`,
///   `reactivated_for_root_spawn = true`).
///
/// `result_summary` is stamped on the task (defaulting to an `"operator retry"`
/// note) and a `task.updated` causal event is emitted.
pub fn retry_workflow_task(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    operator_note: Option<&str>,
) -> anyhow::Result<TaskRun> {
    let mut task = load_task_run(config, store, workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task '{}' not in workflow '{}'", task_id, workflow_id))?;
    anyhow::ensure!(
        matches!(task.status, TaskRunStatus::Failed | TaskRunStatus::Aborted),
        "task '{}' is {:?}; only Failed or Aborted tasks can be operator-retried",
        task_id,
        task.status,
    );

    let note = operator_note
        .map(str::to_string)
        .unwrap_or_else(|| format!("operator retry at {}", now_rfc3339()));

    task.status = TaskRunStatus::Runnable;
    task.updated_at = now_rfc3339();
    task.retry_count = task.retry_count.saturating_add(1);
    task.result_summary = Some(note.clone());
    // Clear the prior failure classification so the retried attempt starts
    // without inherited failure metadata; a new failure will re-populate it.
    task.last_failure_class = None;
    task.side_effect_state = None;
    save_task_run(config, store, &task)?;

    // Reactivate the parent workflow run if it is itself terminal, so the
    // scheduler picks the revived task back up. Mirrors the root-spawn
    // reactivation in `runtime/tools/agent.rs` (status -> Resumable).
    if is_workflow_terminal(config, store, workflow_id)? {
        if let Some(mut run) = load_workflow_run(config, store, workflow_id)? {
            run.status = WorkflowRunStatus::Resumable;
            run.reactivated_for_root_spawn = true;
            run.updated_at = now_rfc3339();
            save_workflow_run(config, store, &run)?;
        }
    }

    // Event emission is best-effort: by this point the task and (optionally)
    // workflow-run state have been durably mutated and the retry has succeeded.
    // Returning an error here would mislead the operator (the retry *did* work)
    // and could cause a double-retry (retry_count bumped again). Log and
    // continue returning the updated task.
    if let Err(e) = append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: Some(task_id.to_string()),
            event_type: "task.updated".to_string(),
            agent_id: Some(task.agent_id.clone()),
            payload: serde_json::json!({
                "status": TaskRunStatus::Runnable,
                "result_summary": note,
                "retry_count": task.retry_count,
                "reason": "operator_retry",
            }),
            occurred_at: now_rfc3339(),
        },
    ) {
        tracing::warn!(
            target: "workflow_store",
            workflow_id,
            task_id,
            error = %e,
            "retry_workflow_task: failed to append task.updated event (state already mutated; retry succeeded)",
        );
    }

    Ok(task)
}

/// Resolve a workflow id from operator-supplied `workflow_id` / `root_session`
/// parameters, applying consistent normalization (trim; whitespace-only treated
/// as absent). Used by both the CLI handler and the `workflow.task.retry` RPC
/// arm so the resolution ladder and error messages stay identical.
///
/// Resolution order: explicit `workflow_id` > lookup by `root_session` > error.
pub fn resolve_workflow_id_for_operator_retry(
    config: &GatewayConfig,
    workflow_id: Option<&str>,
    root_session: Option<&str>,
) -> anyhow::Result<String> {
    let wf = workflow_id.map(str::trim).filter(|s| !s.is_empty());
    let rs = root_session.map(str::trim).filter(|s| !s.is_empty());
    match wf {
        Some(w) => Ok(w.to_string()),
        None => match rs {
            Some(rsid) => resolve_workflow_id_for_root_session(config, rsid)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no workflow found for root session '{}'; pass --workflow-id explicitly",
                        rsid
                    )
                }),
            None => anyhow::bail!(
                "either workflow_id or root_session must be provided to locate the workflow"
            ),
        },
    }
}

/// Gather child-state summaries for all terminal tasks in a join group.
/// Used to enrich the WorkflowJoinSatisfied signal so the planner can see
/// every child's result without separate `workflow_state` / artifact inspect
/// rounds.
fn gather_join_child_summaries(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    join_task_ids: &[String],
) -> Vec<ChildStateNotification> {
    // When join_task_ids is empty, the join checks ALL tasks in the
    // workflow. Gather all terminal tasks in that case.
    let all_tasks = list_task_runs_for_workflow(config, store, workflow_id);
    let task_ids: Vec<String> = if join_task_ids.is_empty() {
        match &all_tasks {
            Ok(tasks) => tasks.iter().map(|t| t.task_id.clone()).collect(),
            Err(_) => return Vec::new(),
        }
    } else {
        join_task_ids.to_vec()
    };

    let task_map: std::collections::HashMap<String, TaskRun> = match &all_tasks {
        Ok(tasks) => tasks
            .iter()
            .map(|t| (t.task_id.clone(), t.clone()))
            .collect(),
        Err(_) => return Vec::new(),
    };

    let mut summaries = Vec::new();
    for tid in &task_ids {
        let Some(task) = task_map.get(tid) else {
            continue;
        };
        if !matches!(
            task.status,
            TaskRunStatus::Succeeded
                | TaskRunStatus::Failed
                | TaskRunStatus::Cancelled
                | TaskRunStatus::Aborted
        ) {
            continue;
        }
        let failure_meta = task
            .last_failure_class
            .map(crate::runtime::failure_classification::metadata_for_failure_class);
        summaries.push(build_child_state_notification(
            task,
            task.status,
            task.result_summary.as_deref(),
            failure_meta.as_ref(),
        ));
    }
    summaries
}

fn child_state_event_type(status: TaskRunStatus) -> Option<&'static str> {
    match status {
        TaskRunStatus::AwaitingApproval | TaskRunStatus::Paused => Some("workflow.child.waiting"),
        TaskRunStatus::Runnable
        | TaskRunStatus::Succeeded
        | TaskRunStatus::Failed
        | TaskRunStatus::Cancelled
        | TaskRunStatus::Aborted => Some("workflow.child.resolved"),
        _ => None,
    }
}

/// RFC #776 Part B.1 — compute whether declared `expected_outputs` are
/// covered by the child's actual production. Purely mechanical: checks
/// name existence against content files and artifact file lists, never
/// quality (invariant 5: check existence, not truth).
///
/// Returns the list of unmet output names (empty = all covered).
pub fn check_output_contract(
    expected_outputs: &[String],
    produced_content_names: &[String],
    produced_artifact_files: &[String],
) -> Vec<String> {
    expected_outputs
        .iter()
        .filter_map(|expected| {
            let name = expected.trim();
            if name.is_empty()
                || produced_content_names.iter().any(|p| p.as_str() == name)
                || produced_artifact_files.iter().any(|p| p.as_str() == name)
            {
                None
            } else {
                // Return the trimmed name so the unmet list is self-consistent
                // with the comparison logic (no whitespace-padding surprises).
                Some(name.to_string())
            }
        })
        .collect()
}

/// RFC #776 Part B.1 — record the output contract check result on a task's
/// metadata so `build_child_state_notification` can stamp
/// `OutputContractUnmet`. Called by the scheduler after child completion,
/// before `update_task_run_status`.
pub fn record_output_contract_check(
    task: &mut TaskRun,
    unmet: Vec<String>,
) {
    let metadata = task.metadata.get_or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "output_contract_check".to_string(),
            serde_json::json!({
                "unmet": unmet,
                "checked_at": now_rfc3339(),
            }),
        );
    }
}

/// Record where a child's full reply was spilled when it exceeded the
/// `result_summary` budget, so `workflow.wait` can hand the parent a handle to
/// the complete payload instead of only the shortened copy.
pub fn record_full_result_ref(task: &mut TaskRun, cnt_ref: &str) {
    let metadata = task.metadata.get_or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "full_result_ref".to_string(),
            serde_json::Value::String(cnt_ref.to_string()),
        );
    }
}

/// The `cnt_` ref of a child's full reply, if it was spilled.
pub fn full_result_ref(task: &TaskRun) -> Option<&str> {
    task.metadata
        .as_ref()?
        .get("full_result_ref")?
        .as_str()
}

fn build_child_state_notification(
    task: &TaskRun,
    status: TaskRunStatus,
    result_summary: Option<&str>,
    task_failure: Option<&crate::runtime::failure_classification::WorkflowFailureMetadata>,
) -> ChildStateNotification {
    // RFC #775 Part A: extract the normalized agent outcome from the summary
    // so the parent can branch on a typed signal instead of re-deriving it
    // from prose. ClarificationNeeded is the key case — penalty-free, not a
    // failure, but the parent needs to know mechanically.
    let agent_outcome = if matches!(status, TaskRunStatus::Succeeded) {
        result_summary
            .and_then(autonoetic_types::task_completion::extract_agent_outcome)
    } else {
        None
    };

    let mut failure_class = task_failure.and_then(|failure| failure.failure_class);

    // RFC #776 Part B.1: check if the output contract was unmet. The
    // scheduler records the check result in the task metadata before
    // calling update_task_run_status. If any declared expected_outputs
    // are missing, stamp OutputContractUnmet (no blind retry — invariant 1).
    if matches!(status, TaskRunStatus::Succeeded) && failure_class.is_none() {
        if let Some(unmet) = task
            .metadata
            .as_ref()
            .and_then(|m| m.get("output_contract_check"))
            .and_then(|v| v.get("unmet"))
            .and_then(|v| v.as_array())
            .filter(|arr| !arr.is_empty())
        {
            let names: Vec<String> = unmet
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !names.is_empty() {
                tracing::info!(
                    target: "workflow",
                    task_id = %task.task_id,
                    unmet = ?names,
                    "Output contract unmet — stamping failure_class"
                );
                failure_class =
                    Some(autonoetic_types::tool_error::FailureClass::OutputContractUnmet);
            }
        }
    }

    // RFC #775 Part A: detect "gave up" — child succeeded but produced no
    // recognizable result or account. This is distinct from Unknown (an
    // unclassifiable error). The child ended cleanly but said nothing useful.
    if matches!(status, TaskRunStatus::Succeeded) && failure_class.is_none() {
        let summary_blank = result_summary
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        let no_outcome = agent_outcome.is_none();
        if summary_blank && no_outcome {
            failure_class = Some(autonoetic_types::tool_error::FailureClass::ChildGaveUp);
        }
    }

    let install_conflict_detail =
        if failure_class == Some(autonoetic_types::tool_error::FailureClass::InstallConflict) {
            result_summary
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .map(ToString::to_string)
        } else {
            None
        };

    // For gave_up, provide retry advice/retryable since there's no
    // task_failure entry (evaluate_stage_retry doesn't fire on Succeeded).
    let gave_up_advice = if failure_class
        == Some(autonoetic_types::tool_error::FailureClass::ChildGaveUp)
    {
        Some(autonoetic_types::tool_error::RetryAdvice::DoNotRetry)
    } else {
        None
    };
    let gave_up_side_effect = if failure_class
        == Some(autonoetic_types::tool_error::FailureClass::ChildGaveUp)
    {
        Some(autonoetic_types::tool_error::SideEffectState::Unknown)
    } else {
        None
    };

    ChildStateNotification {
        workflow_id: task.workflow_id.clone(),
        task_id: task.task_id.clone(),
        child_session_id: task.session_id.clone(),
        child_status: status.as_str().to_string(),
        failure_class,
        install_conflict_detail,
        retry_advice: task_failure
            .and_then(|failure| failure.retry_advice)
            .or(gave_up_advice),
        side_effect_state: task_failure
            .and_then(|failure| failure.side_effect_state)
            .or(gave_up_side_effect),
        agent_outcome,
        summary: result_summary.map(ToString::to_string),
    }
}

/// Attempt to transition a workflow from `Resumable` to `Completed`.
///
/// Called when the root (planner) session closes normally. The transition fires
/// only when the workflow is `Resumable` AND all join tasks are terminal AND no
/// active or queued tasks remain. Returns `true` if the transition occurred.
pub fn try_complete_workflow(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    root_session_id: &str,
) -> anyhow::Result<bool> {
    let wf_id = match resolve_workflow_id_for_root_session(config, root_session_id)? {
        Some(id) => id,
        None => return Ok(false),
    };
    let run = match load_workflow_run(config, store, &wf_id)? {
        Some(r) => r,
        None => return Ok(false),
    };
    if run.status != WorkflowRunStatus::Resumable {
        return Ok(false);
    }
    if !run.active_task_ids.is_empty() || !run.queued_task_ids.is_empty() {
        return Ok(false);
    }
    if !check_join_condition(config, store, &wf_id)? {
        return Ok(false);
    }

    // #742/#1057: don't complete the workflow unless the root session lifecycle
    // indicates the session has no further planned work. The set of states that
    // permit completion is owned by `SessionLifecycleState::permits_workflow_completion`:
    // `terminated:*` (ended), `hibernated` (between turns — completing is fine since
    // all join/active conditions are independently checked above), and `idle`
    // (finished its task, parked only for peer reachability). `awaiting_gate`
    // (an operator decision is pending) and `paused` (operator may redirect)
    // block completion, as does `active`.
    // Pre-migration rows with no lifecycle_state fall through to the legacy
    // pending-escalation check below.
    if let Some(gw_store) = store {
        match gw_store.get_session_lifecycle_state(root_session_id)? {
            Some(ref raw) => match raw.parse::<SessionLifecycleState>() {
                Ok(state) if state.permits_workflow_completion() => { /* proceed */ }
                Ok(state) => {
                    tracing::debug!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        root_session_id = %root_session_id,
                        lifecycle_state = %state,
                        "Workflow not completing: root session lifecycle is {}",
                        state
                    );
                    return Ok(false);
                }
                // An unparseable value: block (safe default) and surface it.
                Err(_) => {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        root_session_id = %root_session_id,
                        lifecycle_state = %raw,
                        "Workflow not completing: unparseable root session lifecycle_state"
                    );
                    return Ok(false);
                }
            },
            // Pre-migration fallback: no lifecycle_state — use legacy checks.
            None => {
                if gw_store.has_pending_escalations_for_session(root_session_id)? {
                    tracing::debug!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        root_session_id = %root_session_id,
                        "Workflow not completing: pending escalation(s) for root session (pre-migration fallback)"
                    );
                    return Ok(false);
                }
            }
        }
    }

    // Issue #330: warn if the workflow has active workbenches with
    // local edits that haven't been reconciled yet. The workflow still
    // completes, but the warning event surfaces the unreconciled edits
    // so the operator can decide whether to reconcile, discard, or
    // archive before starting a new workflow.
    if let Some(gw_store) = store {
        match gw_store.list_workbenches_for_workflow(&wf_id) {
            Ok(workbenches) => {
                let unreconciled: Vec<_> = workbenches
                    .iter()
                    .filter(|wb| wb.status == autonoetic_types::workbench::WorkbenchStatus::Active)
                    .map(|wb| wb.workbench_id.clone())
                    .collect();
                if !unreconciled.is_empty() {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        count = unreconciled.len(),
                        workbench_ids = ?unreconciled,
                        "Workflow completing with {} unreconciled active workbench(es)",
                        unreconciled.len()
                    );
                    if let Err(e) = append_workflow_event(
                        config,
                        Some(gw_store),
                        &WorkflowEventRecord {
                            event_id: new_event_id(),
                            workflow_id: wf_id.clone(),
                            task_id: None,
                            event_type: "workflow.unreconciled_workbenches".to_string(),
                            agent_id: None,
                            payload: serde_json::json!({
                                "root_session_id": root_session_id,
                                "unreconciled_workbench_ids": unreconciled,
                                "message": format!(
                                    "Workflow completed with {} unreconciled active workbench(es). \
                                     Reconcile, discard, or archive before starting a new workflow.",
                                    unreconciled.len()
                                ),
                            }),
                            occurred_at: now_rfc3339(),
                        },
                    ) {
                        tracing::error!(
                            target: "workflow",
                            workflow_id = %wf_id,
                            error = %e,
                            "Failed to append workflow.unreconciled_workbenches event"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    error = %e,
                    "Failed to list workbenches for workflow completion warning"
                );
            }
        }
    }

    let mut wf = run;
    wf.status = WorkflowRunStatus::Completed;
    wf.reactivated_for_root_spawn = false;
    wf.updated_at = now_rfc3339();
    save_workflow_run(config, store, &wf)?;

    // Suppress any pending notifications so stale child-state / join-satisfied
    // signals do not wake the root session after the workflow is complete.
    if let Some(gs) = store {
        match suppress_pending_notifications_for_workflow(Some(gs), &wf_id) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(
                        workflow_id = %wf_id,
                        suppressed_count = count,
                        "Suppressed pending notifications for completed workflow"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    workflow_id = %wf_id,
                    error = %e,
                    "Failed to suppress pending notifications for completed workflow"
                );
            }
        }
    }

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: wf_id.clone(),
            task_id: None,
            event_type: "workflow.completed".to_string(),
            agent_id: None,
            payload: serde_json::json!({
                "root_session_id": root_session_id,
                "join_task_ids": wf.join_task_ids,
            }),
            occurred_at: now_rfc3339(),
        },
    )?;

    tracing::info!(
        target: "workflow",
        workflow_id = %wf_id,
        root_session_id = %root_session_id,
        "Workflow completed — all join tasks terminal, no active/queued work"
    );

    Ok(true)
}

/// Transition any `Running` workflow tasks bound to `session_id` to `Failed`.
///
/// Called from `close_session` when a child session terminates abnormally.
/// Without this, tasks stay `Running` forever because `close_session` only
/// finalizes transcripts — it has no callback into the workflow task layer.
/// The orphan reaper (R+12) only fires when the *parent* is terminal, so
/// a child that dies while the parent is alive/suspended leaves tasks stuck.
pub fn fail_running_tasks_for_session(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    session_id: &str,
    reason: &str,
) -> anyhow::Result<usize> {
    let root = crate::runtime::content_store::root_session_id(session_id);
    let wf_id = match resolve_workflow_id_for_root_session(config, &root)? {
        Some(id) => id,
        None => return Ok(0),
    };
    let tasks = list_task_runs_for_workflow(config, store, &wf_id)?;
    let mut failed = 0usize;
    for task in tasks {
        if task.session_id == session_id && task.status == TaskRunStatus::Running {
            let task_id = task.task_id.clone();
            let summary = format!("child session terminated: {}", reason);
            match update_task_run_status(
                config,
                store,
                &wf_id,
                &task_id,
                TaskRunStatus::Failed,
                Some(summary),
                None,
                None,
            ) {
                Ok(()) => {
                    failed += 1;
                    tracing::info!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        task_id = %task_id,
                        session_id = %session_id,
                        failed_count = failed,
                        "Transitioned Running task to Failed after child session termination"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        task_id = %task_id,
                        error = %e,
                        "Failed to transition Running task to Failed after session termination"
                    );
                }
            }
        }
    }
    Ok(failed)
}

/// Fail all non-terminal tasks in the workflow for a root session. Called when
/// the root session closes with an error. If `mark_workflow_failed` is true,
/// the workflow is also marked Failed (GAP-1B). When false, only the tasks are
/// failed so the workflow itself stays recoverable while ensuring tasks don't
/// stay Running forever against a dead root.
///
/// Writes task status directly (like `apply_emergency_stop_to_workflow`) rather
/// than going through `update_task_run_status` to avoid emitting misleading
/// join-satisfied signals and child-state notifications to an already-dead root.
pub fn fail_workflow_for_root_session(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    root_session_id: &str,
    reason: &str,
    mark_workflow_failed: bool,
) -> anyhow::Result<bool> {
    let wf_id = match resolve_workflow_id_for_root_session(config, root_session_id)? {
        Some(id) => id,
        None => return Ok(false),
    };
    let tasks = list_task_runs_for_workflow(config, store, &wf_id)?;
    let mut failed = 0usize;
    let now = now_rfc3339();
    for mut task in tasks {
        // The orphan reaper protects `Stale` (resumable per #722 Stage 2) by
        // treating it as active-and-protected. Use `is_terminal()` which
        // excludes `Stale`.
        let is_terminal = task.status.is_terminal();
        if is_terminal {
            continue;
        }
        task.status = TaskRunStatus::Failed;
        task.updated_at = now.clone();
        task.result_summary = Some(format!("root session terminated: {}", reason));
        if let Err(e) = save_task_run(config, store, &task) {
            tracing::warn!(
                target: "workflow",
                workflow_id = %wf_id,
                task_id = %task.task_id,
                error = %e,
                "Failed to write Failed status during workflow failure"
            );
            continue;
        }
        failed += 1;

        // Terminate the bound session transcript (mirrors GAP-2A in
        // update_task_run_status, but without the signal emission that would
        // wake an already-dead root). The root is gone, so a resumable
        // lifecycle on this child would only feed the orphan reaper forever.
        if !task.session_id.is_empty() {
            if let Some(store) = store {
                if let Err(e) = store.terminate_session_transcript(&task.session_id, &now, "failed")
                {
                    tracing::warn!(
                        target: "workflow",
                        session_id = %task.session_id,
                        error = %e,
                        "Failed to finalize session transcript during workflow failure"
                    );
                }
            }
        }
    }

    // Mark the workflow itself as Failed only when requested — and never when
    // the workflow was emergency-stopped: the stop already aborted every task
    // and set the status, and a root-session error racing the stop (the
    // in-flight turn's error landing after the stop handler ran) must not flip
    // the visible status from EmergencyStopped to Failed (session-d1d8c2bb
    // churn: "Workflow failed — root session terminated with error" repeatedly
    // after the operator's stop).
    if mark_workflow_failed {
        if let Ok(Some(mut wf)) = load_workflow_run(config, store, &wf_id) {
            if matches!(
                wf.status,
                WorkflowRunStatus::EmergencyStopped | WorkflowRunStatus::EmergencyStopping
            ) {
                tracing::info!(
                    target: "workflow",
                    workflow_id = %wf_id,
                    status = ?wf.status,
                    "Root session error after emergency stop: keeping EmergencyStopped status (stop wins)"
                );
            } else {
                wf.status = WorkflowRunStatus::Failed;
                wf.updated_at = now_rfc3339();
                if let Err(e) = save_workflow_run(config, store, &wf) {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %wf_id,
                        error = %e,
                        "Failed to mark workflow as Failed after root session error"
                    );
                }
            }
        }
    }

    // Suppress pending notifications so an already-dead root is not woken.
    if let Some(gs) = store {
        if let Err(e) = suppress_pending_notifications_for_workflow(Some(gs), &wf_id) {
            tracing::warn!(
                target: "workflow",
                workflow_id = %wf_id,
                error = %e,
                "Failed to suppress pending notifications after workflow failure"
            );
        }
    }

    let status_msg = if mark_workflow_failed {
        "Workflow failed"
    } else {
        "Workflow tasks failed (workflow left recoverable)"
    };
    tracing::info!(
        target: "workflow",
        workflow_id = %wf_id,
        root_session_id = %root_session_id,
        failed_count = failed,
        mark_workflow_failed,
        "{} — root session terminated with error",
        status_msg
    );
    Ok(true)
}

/// Creates an implicit artifact reference for a completed task.
///
/// Per spec (spec-implicit-artifacts-agent-evolution.md §4.2), the implicit
/// artifact carries a `content.named_outputs` array listing every file the
/// child agent persisted via `content.write`. This lets the parent (planner)
/// find child outputs via `content.read` with the name or `cnt_` ref —
/// without needing to know the child's internal filenames in advance.
fn create_implicit_artifact(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    task: &TaskRun,
    result_summary: Option<&str>,
) -> anyhow::Result<()> {
    use crate::runtime::content_store::{ContentStore, ContentVisibility};

    let gw_dir = crate::execution::gateway_root_dir(config);
    let content_store = ContentStore::new(&gw_dir)?;

    // Generate implicit artifact ID
    let artifact_id = format!("impl_{}", task.task_id);
    let parent_session = &task.parent_session_id;

    // Collect child outputs that the parent session can actually resolve.
    // Each entry gives the parent a `name` (e.g. "weather_api_research.md") and
    // a `ref` (e.g. "cnt_a1b2c3d4") that can be passed directly to content.read.
    let named_outputs: Vec<serde_json::Value> = content_store
        .list_names_with_handles(&task.session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, handle)| {
            content_store
                .is_handle_visible(parent_session, handle)
                .unwrap_or(false)
        })
        .map(|(name, handle)| {
            let short_ref = format!("cnt_{}", ContentStore::get_short_alias(&handle));
            serde_json::json!({
                "name": name,
                "ref": short_ref,
            })
        })
        .collect();

    let named_outputs_count = named_outputs.len();

    // Collect built artifacts for this child session and expose enough
    // metadata for agents to pick the right one without gateway-side heuristics.
    let artifact_store = crate::artifact_store::ArtifactStore::new(&gw_dir);
    let refs_by_artifact_id: std::collections::HashMap<String, String> = gateway_store
        .map(|gs| {
            let mut refs = std::collections::HashMap::new();
            for (scope_type, scope_id) in [
                (
                    autonoetic_types::artifact::ArtifactRefScopeType::Global,
                    "__global__",
                ),
                (
                    autonoetic_types::artifact::ArtifactRefScopeType::Workflow,
                    task.workflow_id.as_str(),
                ),
                (
                    autonoetic_types::artifact::ArtifactRefScopeType::Session,
                    task.session_id.as_str(),
                ),
            ] {
                for record in gs
                    .list_artifact_refs_for_scope(scope_type, scope_id)
                    .unwrap_or_default()
                {
                    refs.entry(record.artifact_id).or_insert(record.ref_id);
                }
            }
            refs
        })
        .unwrap_or_default();
    let built_artifacts: Vec<serde_json::Value> = artifact_store
        .map(|store| {
            let mut items: Vec<serde_json::Value> = store
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|art_id| {
                    let bundle = store.inspect(&art_id).ok()?;
                    if bundle.builder_session_id != task.session_id {
                        return None;
                    }
                    let artifact_ref = refs_by_artifact_id.get(&bundle.artifact_id).cloned();
                    Some(serde_json::json!({
                        "artifact_ref": artifact_ref,
                        "artifact_canonical_digest": bundle.artifact_canonical_digest,
                        "kind": bundle.kind,
                        "entrypoints": bundle.entrypoints,
                        "file_count": bundle.files.len(),
                        "created_at": bundle.created_at,
                    }))
                })
                .collect();
            items.sort_by(|a, b| {
                let aid = a
                    .get("artifact_ref")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let bid = b
                    .get("artifact_ref")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                aid.cmp(bid)
            });
            items
        })
        .unwrap_or_default();

    // Build implicit artifact — spec §4.2 structure.
    let implicit_data = serde_json::json!({
        "implicit_artifact_id": artifact_id,
        "artifact_type": "implicit",
        "task_id": task.task_id,
        "agent_id": task.agent_id,
        "session_id": task.session_id,
        "parent_session": task.parent_session_id,
        "created_at": task.updated_at,
        "summary": result_summary.unwrap_or("Task completed"),
        "content": {
            // Named outputs from the child session — use name or ref with content.read
            "named_outputs": named_outputs,
            // Executable artifacts built in the child session via artifact.build.
            // Gateway intentionally does not choose one; agents decide using
            // metadata such as kind/entrypoints/file_count.
            "artifacts": built_artifacts,
        },
    });

    // Write as session-visible content in parent session
    let json_bytes = serde_json::to_vec_pretty(&implicit_data)?;
    let handle = content_store.write(&json_bytes)?;

    // Register with session visibility for parent session access
    content_store.register_name_with_visibility(
        parent_session,
        &artifact_id,
        &handle,
        ContentVisibility::Session,
    )?;

    tracing::debug!(
        target: "workflow",
        task_id = %task.task_id,
        artifact_id = %artifact_id,
        parent_session = %parent_session,
        named_outputs = named_outputs_count,
        "Created implicit artifact for completed task"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Async task queue (SQLite-backed; legacy files are migrated on first touch)
// ---------------------------------------------------------------------------

fn queue_dir(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id).join("queue")
}

fn queued_task_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    queue_dir(config, workflow_id).join(format!("{task_id}.json"))
}

pub(crate) fn task_claim_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    queue_dir(config, workflow_id).join(format!("{task_id}.claim.json"))
}

fn parse_rfc3339(ts: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(ts)?.with_timezone(&Utc))
}

pub(crate) fn claim_is_stale(claim: &TaskExecutionClaim, stale_after_secs: u64) -> bool {
    let Ok(heartbeat_at) = parse_rfc3339(&claim.heartbeat_at) else {
        return true;
    };
    Utc::now() - heartbeat_at > Duration::seconds(stale_after_secs as i64)
}

fn open_store_for<'a>(
    config: &GatewayConfig,
    store: Option<&'a GatewayStore>,
    owned: &'a mut Option<GatewayStore>,
) -> anyhow::Result<&'a GatewayStore> {
    if let Some(s) = store {
        return Ok(s);
    }
    let gateway_dir = crate::execution::gateway_root_dir(config);
    *owned = Some(GatewayStore::open(&gateway_dir)?);
    Ok(owned.as_ref().unwrap())
}

pub fn load_task_claim(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskExecutionClaim>> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    if let Some(claim) = store.get_task_claim(workflow_id, task_id)? {
        return Ok(Some(claim));
    }
    let path = task_claim_path(config, workflow_id, task_id);
    if !path.exists() {
        return Ok(None);
    }
    let claim: TaskExecutionClaim = read_json_file(&path)?;
    store.upsert_task_claim(&claim)?;
    let _ = fs::remove_file(&path);
    Ok(Some(claim))
}

pub fn acquire_task_claim(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    stale_after_secs: u64,
) -> anyhow::Result<Option<TaskExecutionClaim>> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;

    // If there is a legacy claim file, migrate it first so SQLite owns the claim.
    // Only remove the file once the claim is durably stored in SQLite — otherwise
    // a failed upsert would drop the only record of an active claim and risk
    // duplicate execution.
    let path = task_claim_path(config, workflow_id, task_id);
    if path.exists() {
        match read_json_file::<TaskExecutionClaim>(&path) {
            Ok(legacy) => match store.upsert_task_claim(&legacy) {
                Ok(()) => {
                    let _ = fs::remove_file(&path);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to migrate legacy task claim into SQLite; leaving file in place",
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skip invalid legacy task claim json; leaving file in place",
                );
            }
        }
    }

    store.acquire_task_claim(workflow_id, task_id, stale_after_secs)
}

pub fn refresh_task_claim_heartbeat(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    store.refresh_task_claim_heartbeat(workflow_id, task_id)
}

pub fn release_task_claim(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    store.delete_task_claim(workflow_id, task_id)?;
    // Also remove legacy file if it exists.
    let path = task_claim_path(config, workflow_id, task_id);
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    Ok(())
}

/// Refresh a running task's `updated_at` without changing status or emitting workflow events.
///
/// This is used as a low-noise heartbeat while a long-running execution is still active
/// (e.g. synchronous `agent.spawn` blocked on post-processing). It prevents false
/// stuck-task detection based purely on stale `updated_at`.
pub fn refresh_task_run_heartbeat(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let Some(mut task) = load_task_run(config, store, workflow_id, task_id)? else {
        return Ok(());
    };
    if task.status != TaskRunStatus::Running {
        return Ok(());
    }
    task.updated_at = now_rfc3339();
    save_task_run(config, store, &task)
}

pub fn queued_task_exists(config: &GatewayConfig, store: Option<&GatewayStore>, workflow_id: &str, task_id: &str) -> bool {
    let mut owned_store = None;
    if let Ok(store) = open_store_for(config, store, &mut owned_store) {
        if let Ok(Some(_)) = store.get_queued_task(workflow_id, task_id) {
            return true;
        }
    }
    queued_task_path(config, workflow_id, task_id).exists()
}

/// When a queue file already exists but the task checkpoint has a newer
/// approval_resolved payload, update the queued task message in place.
pub fn refresh_queued_task_message_from_task_checkpoint(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    if !queued_task_exists(config, store, workflow_id, task_id) {
        return Ok(());
    }
    let Some(cp) = load_task_checkpoint(config, store, workflow_id, task_id)? else {
        return Ok(());
    };
    if cp.step != "approval_resolved" {
        return Ok(());
    }
    let resume_raw = cp.state.get("request_id");
    let rm = match resume_raw {
        Some(serde_json::Value::String(s)) => {
            let action = cp
                .state
                .get("action_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = cp
                .state
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            format!("approval_resumed:{}:{}:{}", action, s, status)
        }
        Some(v) => serde_json::to_string(v)?,
        None => return Ok(()),
    };
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    let mut q = match store.get_queued_task(workflow_id, task_id)? {
        Some(q) => q,
        None => return Ok(()),
    };
    if q.message != rm {
        q.message = rm;
        store.enqueue_queued_task(&q)?;
        tracing::info!(
            target: "workflow",
            workflow_id = %workflow_id,
            task_id = %task_id,
            "Refreshed queued task message from approval task checkpoint"
        );
    }
    Ok(())
}

/// Enqueue a task for async execution by the scheduler.
/// Also updates the workflow's `queued_task_ids` and `join_task_ids`.
pub fn enqueue_task(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    queued: &QueuedTaskRun,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;

    // Decide whether the workflow accepts work BEFORE committing the queue row
    // (#883). The drain path reads the queued-tasks table, not the run record's
    // `queued_task_ids`, so a row written ahead of a refusal is executable: work
    // the gateway explicitly refused during an emergency stop would run as soon
    // as the stop lifted, and the caller — told the enqueue failed — would have
    // no reason to look for it.
    let mut run = load_workflow_run(config, Some(store), &queued.workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", queued.workflow_id))?;
    anyhow::ensure!(
        run.status != WorkflowRunStatus::EmergencyStopping
            && run.status != WorkflowRunStatus::EmergencyStopped,
        "workflow '{}' is in emergency stop; refusing new queued work",
        queued.workflow_id
    );

    store.enqueue_queued_task(queued)?;
    // Remove legacy file if present.
    let path = queued_task_path(config, &queued.workflow_id, &queued.task_id);
    if path.exists() {
        let _ = fs::remove_file(&path);
    }

    if !run.queued_task_ids.contains(&queued.task_id) {
        run.queued_task_ids.push(queued.task_id.clone());
    }
    if queued.blocks_planner && !run.join_task_ids.contains(&queued.task_id) {
        run.join_task_ids.push(queued.task_id.clone());
    }
    // Set workflow to WaitingChildren when blocking tasks are enqueued
    if queued.blocks_planner && run.status == WorkflowRunStatus::Active {
        run.status = WorkflowRunStatus::WaitingChildren;
    }
    run.updated_at = now_rfc3339();
    if let Err(e) = save_workflow_run(config, Some(store), &run) {
        // The row is committed but the run record does not list the task. Roll
        // the row back so the two views cannot disagree — a queued-but-
        // unregistered task is drainable while invisible in the run (#883).
        if let Err(rb) = store.dequeue_queued_task(&queued.workflow_id, &queued.task_id) {
            tracing::error!(
                target: "workflow",
                workflow_id = %queued.workflow_id,
                task_id = %queued.task_id,
                error = %rb,
                "Failed to roll back queued task after workflow-run save failure; \
                 the queue row may be drainable without being registered on the run"
            );
        }
        return Err(e);
    }

    // The task is committed and registered by this point, so a timeline-write
    // failure must not be reported as an enqueue failure: the caller would
    // believe nothing was queued while the task actually runs. Degrade to an
    // observability gap instead.
    if let Err(e) = append_workflow_event(
        config,
        Some(store),
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: queued.workflow_id.clone(),
            task_id: Some(queued.task_id.clone()),
            event_type: "task.queued".to_string(),
            agent_id: Some(queued.agent_id.clone()),
            payload: serde_json::json!({
                "agent_id": queued.agent_id,
                "child_session_id": queued.child_session_id,
                "parent_session_id": queued.parent_session_id,
                "blocks_planner": queued.blocks_planner,
            }),
            occurred_at: now_rfc3339(),
        },
    ) {
        tracing::warn!(
            target: "workflow",
            workflow_id = %queued.workflow_id,
            task_id = %queued.task_id,
            error = %e,
            "Task enqueued but the task.queued timeline event could not be written"
        );
    }

    tracing::info!(
        target: "workflow",
        workflow_id = %queued.workflow_id,
        task_id = %queued.task_id,
        agent_id = %queued.agent_id,
        "Task enqueued for async execution"
    );
    Ok(())
}

/// Dequeue (remove from queue) after task execution completes.
pub fn dequeue_task(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    store.dequeue_queued_task(workflow_id, task_id)?;
    // Remove legacy file if present.
    let path = queued_task_path(config, workflow_id, task_id);
    if path.exists() {
        let _ = fs::remove_file(&path);
    }

    let mut run = match load_workflow_run(config, Some(store), workflow_id)? {
        Some(r) => r,
        None => return Ok(()),
    };
    run.queued_task_ids.retain(|id| id != task_id);
    // Also remove from active_task_ids when task completes (bug fix: tasks were staying active forever)
    run.active_task_ids.retain(|id| id != task_id);
    run.updated_at = now_rfc3339();
    save_workflow_run(config, Some(store), &run)?;
    release_task_claim(config, Some(store), workflow_id, task_id)?;
    Ok(())
}

/// Load all queued tasks for a workflow.
pub fn load_queued_tasks(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<QueuedTaskRun>> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;
    migrate_legacy_queued_files(config, store, workflow_id)?;
    store.list_queued_tasks_for_workflow(workflow_id)
}

fn migrate_legacy_queued_files(
    config: &GatewayConfig,
    store: &GatewayStore,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let dir = queue_dir(config, workflow_id);
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.ends_with(".claim.json"))
        {
            continue;
        }
        match read_json_file::<QueuedTaskRun>(&path) {
            Ok(q) => match store.enqueue_queued_task(&q) {
                Ok(()) => {
                    let _ = fs::remove_file(&path);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to migrate legacy queued task json into SQLite; leaving file in place",
                    );
                }
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip invalid legacy queued task json");
            }
        }
    }
    Ok(())
}

/// Load ALL queued tasks across all workflows (for the scheduler tick).
/// Reads from SQLite; legacy queue files are migrated when a workflow is first
/// touched by `load_queued_tasks`.
pub fn load_all_queued_tasks(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
) -> anyhow::Result<Vec<QueuedTaskRun>> {
    let mut owned_store = None;
    let store = open_store_for(config, store, &mut owned_store)?;

    // Migrate any workflow that still has legacy queue files.
    let root = workflows_root(config).join("runs");
    if root.is_dir() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let wf_id = entry.file_name().to_string_lossy().to_string();
            let _ = migrate_legacy_queued_files(config, store, &wf_id);
        }
    }

    store.list_all_queued_tasks()
}

/// Check if a workflow's join condition is satisfied.
/// Respects `join_group`: tasks in the same group are awaited together.
/// Returns true when ANY group has all its tasks in terminal status.
/// This means different groups are independent — whichever finishes first
/// triggers the planner resume. Tasks without a join_group are treated as
/// belonging to a default group.
pub fn check_join_condition(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    let run = match load_workflow_run(config, store, workflow_id)? {
        Some(r) => r,
        None => return Ok(false),
    };
    if run.join_task_ids.is_empty() {
        return Ok(true);
    }

    // Group join tasks by their join_group field
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for task_id in &run.join_task_ids {
        let group = match load_task_run(config, store, workflow_id, task_id)? {
            Some(task) => task.join_group.unwrap_or_default(),
            None => return Ok(false),
        };
        groups.entry(group).or_default().push(task_id.clone());
    }

    // Check each group: if ALL tasks in ANY group are terminal, join is satisfied.
    // `Stale` counts as terminal for joins (unblocks the join) but not for
    // session finalization (see #722 Stage 2 / P-2.11).
    for (_group, task_ids) in &groups {
        let mut all_terminal = true;
        for task_id in task_ids {
            match load_task_run(config, store, workflow_id, task_id)? {
                Some(task) => {
                    if task.status.is_terminal_for_join() {
                        continue;
                    }
                    all_terminal = false;
                    break;
                }
                None => {
                    all_terminal = false;
                    break;
                }
            }
        }
        if all_terminal {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone)]
pub struct EmergencyStopWorkflowSummary {
    pub workflow_id: String,
    pub tasks_aborted: u32,
    pub queued_removed: u32,
    pub already_stopped: bool,
}

/// Durably halt a workflow: dequeue work, abort non-terminal tasks, mark workflow
/// [`WorkflowRunStatus::EmergencyStopped`].
///
/// In-memory tokio work must be aborted separately (see scheduler + active execution registry).
pub fn apply_emergency_stop_to_workflow(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    stop_id: &str,
) -> anyhow::Result<EmergencyStopWorkflowSummary> {
    let mut run = load_workflow_run(config, store, workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", workflow_id))?;

    if run.status == WorkflowRunStatus::EmergencyStopped {
        return Ok(EmergencyStopWorkflowSummary {
            workflow_id: workflow_id.to_string(),
            tasks_aborted: 0,
            queued_removed: 0,
            already_stopped: true,
        });
    }

    let root_sid = run.root_session_id.clone();

    run.status = WorkflowRunStatus::EmergencyStopping;
    run.updated_at = now_rfc3339();
    save_workflow_run(config, store, &run)?;

    let queued = load_queued_tasks(config, store, workflow_id)?;
    let mut queued_removed = 0u32;
    for q in queued {
        dequeue_task(config, store, workflow_id, &q.task_id)?;
        let _ = release_task_claim(config, store, workflow_id, &q.task_id);
        queued_removed += 1;
    }

    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    let mut tasks_aborted = 0u32;
    for mut task in tasks {
        // Emergency stop aborts every non-fully-terminal task, including
        // `Stale` (which is resumable per #722 Stage 2 but should not survive
        // an emergency stop).
        if task.status.is_terminal() {
            continue;
        }
        let _ = release_task_claim(config, store, workflow_id, &task.task_id);

        task.status = TaskRunStatus::Aborted;
        task.updated_at = now_rfc3339();
        task.result_summary = Some(format!("emergency_stop:{stop_id}"));
        save_task_run(config, store, &task)?;

        // GAP-1C: terminate the bound session transcript so it doesn't
        // leak as 'active' forever. The orphan reaper can't see it
        // because the parent session is also emergency-stopped (active).
        // An emergency stop cancels this task's pending gates, so a
        // resumable `awaiting_gate`/`hibernated` lifecycle must not
        // survive it — hence terminate, not the polite finalize.
        if !task.session_id.is_empty() {
            if let Some(store) = store {
                let now = now_rfc3339();
                if let Err(e) = store.terminate_session_transcript(&task.session_id, &now, "failed")
                {
                    tracing::warn!(
                        target: "workflow",
                        workflow_id = %workflow_id,
                        task_id = %task.task_id,
                        session_id = %task.session_id,
                        error = %e,
                        "Failed to finalize session transcript during emergency stop"
                    );
                }
            }
        }

        append_workflow_event(
            config,
            store,
            &WorkflowEventRecord {
                event_id: new_event_id(),
                workflow_id: workflow_id.to_string(),
                task_id: Some(task.task_id.clone()),
                event_type: "task.failed".to_string(),
                agent_id: Some(task.agent_id.clone()),
                payload: serde_json::json!({ "status": "aborted", "stop_id": stop_id }),
                occurred_at: now_rfc3339(),
            },
        )?;

        crate::scheduler::workflow_causal::mirror_orchestration_event(
            config,
            &root_sid,
            "workflow.task.failed",
            EntryStatus::Error,
            serde_json::json!({
                "workflow_id": workflow_id,
                "task_id": task.task_id,
                "workflow_event_type": "task.failed",
                "target_agent_id": task.agent_id,
                "child_session_id": task.session_id,
                "parent_session_id": task.parent_session_id,
                "emergency_stop": stop_id,
            }),
        );
        tasks_aborted += 1;
    }

    run.queued_task_ids.clear();
    run.status = WorkflowRunStatus::EmergencyStopped;
    run.updated_at = now_rfc3339();
    save_workflow_run(config, store, &run)?;

    // Clear singleton index so future workflows are not blocked.
    if let Some(store) = store {
        if let Err(e) = store.delete_singleton_slots_for_workflow(workflow_id) {
            tracing::warn!(
                target: "singleton_dedup",
                workflow_id = %workflow_id,
                stop_id = %stop_id,
                error = %e,
                "Failed to delete singleton slots during emergency stop"
            );
        }
    }

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: None,
            event_type: "workflow.emergency_stopped".to_string(),
            agent_id: None,
            payload: serde_json::json!({ "stop_id": stop_id }),
            occurred_at: now_rfc3339(),
        },
    )?;
    if let Err(e) = refresh_workflow_graph_markdown(config, store, workflow_id) {
        tracing::warn!(
            target: "workflow",
            error = %e,
            "Failed to refresh workflow graph after emergency stop"
        );
    }

    Ok(EmergencyStopWorkflowSummary {
        workflow_id: workflow_id.to_string(),
        tasks_aborted,
        queued_removed,
        already_stopped: false,
    })
}

/// Generate a compact summary of a workflow's current state.
/// Returns `None` if no workflow exists for the given root session.
/// Injected into the planner **system** prompt at hibernate; lifecycle also derives a
/// user-visible assistant line for transcripts and JSON-RPC `assistant_reply`.
pub fn compact_workflow_summary(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
) -> anyhow::Result<Option<String>> {
    let wf_id = match resolve_workflow_id_for_root_session(config, root_session_id)? {
        Some(id) => id,
        None => return Ok(None),
    };
    let run = match load_workflow_run(config, store, &wf_id)? {
        Some(r) => r,
        None => return Ok(None),
    };
    let tasks = list_task_runs_for_workflow(config, store, &wf_id)?;
    let queued = load_queued_tasks(config, store, &wf_id)?;

    let mut running = 0usize;
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut other = 0usize;
    for t in &tasks {
        match t.status {
            TaskRunStatus::Running | TaskRunStatus::Runnable => running += 1,
            TaskRunStatus::Succeeded => succeeded += 1,
            TaskRunStatus::Failed | TaskRunStatus::Cancelled | TaskRunStatus::Aborted => {
                failed += 1
            }
            _ => other += 1,
        }
    }

    let total = tasks.len() + queued.len();
    if total == 0 {
        return Ok(None);
    }

    let mut parts = Vec::new();
    parts.push(format!("workflow {}", &wf_id));
    if running > 0 {
        parts.push(format!("{} running", running));
    }
    if !queued.is_empty() {
        parts.push(format!("{} queued", queued.len()));
    }
    if succeeded > 0 {
        parts.push(format!("{} done", succeeded));
    }
    if failed > 0 {
        parts.push(format!("{} failed", failed));
    }
    if other > 0 {
        parts.push(format!("{} other", other));
    }

    let status_str = match run.status {
        WorkflowRunStatus::Resumable => " [RESUMABLE]",
        WorkflowRunStatus::BlockedApproval => " [BLOCKED]",
        WorkflowRunStatus::WaitingChildren => " [WAITING]",
        WorkflowRunStatus::EmergencyStopping => " [EMERGENCY_STOPPING]",
        WorkflowRunStatus::EmergencyStopped => " [EMERGENCY_STOPPED]",
        _ => "",
    };

    let mut summary = format!("{}{}", parts.join(" · "), status_str);

    // Consume planner checkpoint on resume: append the last delegation intent
    if run.status == WorkflowRunStatus::Resumable {
        if let Ok(Some(cp)) = load_workflow_checkpoint(config, store, &wf_id) {
            if !cp.planner_intent.is_empty() {
                summary.push_str(&format!(
                    "\n  last intent (v{}): {}",
                    cp.version, cp.planner_intent
                ));
            }
        }
    }

    Ok(Some(summary))
}

// ---------------------------------------------------------------------------
// In-process workflow event stream (Phase 6)
// ---------------------------------------------------------------------------

use std::sync::mpsc;

/// Handle for an in-process subscription to workflow events.
/// Events are delivered via a `std::sync::mpsc` channel.
pub struct WorkflowEventStream {
    pub workflow_id: String,
    pub root_session_id: String,
    receiver: mpsc::Receiver<WorkflowEventRecord>,
    _poller: std::thread::JoinHandle<()>,
}

impl WorkflowEventStream {
    /// Start streaming events for a workflow. Polls the SQLite workflow store at the given interval.
    pub fn start(
        config: GatewayConfig,
        workflow_id: String,
        root_session_id: String,
        poll_secs: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let wf_id = workflow_id.clone();
        let poller = std::thread::spawn(move || {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(poll_secs.max(1)));
                match load_workflow_events(&config, None, &wf_id) {
                    Ok(events) => {
                        for event in events {
                            if !seen.insert(event.event_id.clone()) {
                                continue;
                            }
                            if tx.send(event).is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                    Err(_) => {
                        // Keep polling on transient store errors.
                    }
                }
            }
        });
        Self {
            workflow_id,
            root_session_id,
            receiver: rx,
            _poller: poller,
        }
    }

    /// Try to receive the next event without blocking.
    pub fn try_recv(&self) -> Option<WorkflowEventRecord> {
        self.receiver.try_recv().ok()
    }

    /// Receive the next event, blocking until one arrives.
    pub fn recv(&self) -> Result<WorkflowEventRecord, mpsc::RecvError> {
        self.receiver.recv()
    }
}

/// Resolve a task_id from a session_id within a workflow.
/// Scans all task runs in the workflow for a matching session_id.
pub fn resolve_task_id_for_session(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    session_id: &str,
) -> anyhow::Result<Option<String>> {
    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    for task in tasks {
        if task.session_id == session_id {
            return Ok(Some(task.task_id));
        }
    }
    // Also check queued tasks
    let queued = load_queued_tasks(config, store, workflow_id)?;
    for q in queued {
        if q.child_session_id == session_id {
            return Ok(Some(q.task_id));
        }
    }
    Ok(None)
}

/// When [`reroute_chat_ingest_for_active_workflow_child_session`] applies, callers should send
/// user chat to the workflow root and optional workflow [`WorkflowRun::lead_agent_id`].
#[derive(Debug, Clone)]
pub struct ChatIngestWorkflowReroute {
    pub root_session_id: String,
    pub workflow_id: String,
    pub lead_agent_id: Option<String>,
}

fn workflow_run_is_active_for_user_chat_routing(run: &WorkflowRun) -> bool {
    // EmergencyStopping is not terminal (the stop is still converging) but it
    // must not attract user chat either — routing a message into a workflow
    // that is actively being torn down was the pre-#740 behavior and must be
    // preserved (#747 review).
    !run.status.is_terminal()
        && !matches!(run.status, WorkflowRunStatus::EmergencyStopping)
}

fn session_matches_child_task_or_queue(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    run: &WorkflowRun,
    session_id: &str,
) -> anyhow::Result<bool> {
    if session_id == run.root_session_id {
        return Ok(false);
    }
    let tasks = list_task_runs_for_workflow(config, store, workflow_id)?;
    if tasks.iter().any(|t| t.session_id == session_id) {
        return Ok(true);
    }
    let queued = load_queued_tasks(config, store, workflow_id)?;
    Ok(queued.iter().any(|q| q.child_session_id == session_id))
}

/// If `session_id` is a **child** delegation session inside a **non-terminal** workflow, return
/// the workflow root session and lead agent so `event.ingest` chat can target the planner.
///
/// When the session is already a workflow root (present in the workflow index), returns `None`.
/// This scans persisted runs under `workflows/runs/` (typically few rows per gateway).
pub fn reroute_chat_ingest_for_active_workflow_child_session(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    session_id: &str,
) -> anyhow::Result<Option<ChatIngestWorkflowReroute>> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Ok(None);
    }

    if resolve_workflow_id_for_root_session(config, session_id)?.is_some() {
        return Ok(None);
    }

    let runs_root = workflows_root(config).join("runs");
    if !runs_root.is_dir() {
        return Ok(None);
    }
    for entry in fs::read_dir(&runs_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let workflow_id = entry.file_name().to_string_lossy().to_string();
        let run = match load_workflow_run(config, store, &workflow_id) {
            Ok(Some(run)) => run,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(
                    target: "workflow_store",
                    workflow_id = %workflow_id,
                    error = %e,
                    "skipping corrupt workflow.json during chat ingest reroute"
                );
                continue;
            }
        };
        if !workflow_run_is_active_for_user_chat_routing(&run) {
            continue;
        }
        if !session_matches_child_task_or_queue(config, store, &workflow_id, &run, session_id)? {
            continue;
        }
        let lead = run.lead_agent_id.trim();
        return Ok(Some(ChatIngestWorkflowReroute {
            root_session_id: run.root_session_id.clone(),
            workflow_id,
            lead_agent_id: if lead.is_empty() {
                None
            } else {
                Some(lead.to_string())
            },
        }));
    }
    Ok(None)
}

/// Check if the workflow graph has been updated since the given timestamp.
/// Returns true if any workflow events were emitted after `since`.
pub fn workflow_updated_since(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
    since: &str,
) -> bool {
    let wf_id = match resolve_workflow_id_for_root_session(config, root_session_id) {
        Ok(Some(id)) => id,
        _ => return false,
    };
    let events = match load_workflow_events(config, store, &wf_id) {
        Ok(e) => e,
        Err(_) => return false,
    };
    events.iter().any(|e| e.occurred_at.as_str() > since)
}

// ---------------------------------------------------------------------------
// Durable checkpoints (Phase 3)
// ---------------------------------------------------------------------------

fn checkpoints_dir(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id).join("checkpoints")
}

fn workflow_checkpoint_path(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    checkpoints_dir(config, workflow_id).join("planner.json")
}

fn task_checkpoint_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    checkpoints_dir(config, workflow_id).join(format!("{task_id}.json"))
}

/// Save a planner-level checkpoint. Increments the version automatically.
pub fn save_workflow_checkpoint(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    checkpoint: &WorkflowCheckpoint,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config, &checkpoint.workflow_id);
    fs::create_dir_all(&dir)?;
    write_json_file(
        &workflow_checkpoint_path(config, &checkpoint.workflow_id),
        checkpoint,
    )?;
    tracing::info!(
        target: "workflow",
        workflow_id = %checkpoint.workflow_id,
        version = checkpoint.version,
        "Saved planner checkpoint"
    );
    Ok(())
}

/// Load the latest planner checkpoint for a workflow.
pub fn load_workflow_checkpoint(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowCheckpoint>> {
    let path = workflow_checkpoint_path(config, workflow_id);
    if !path.exists() {
        return Ok(None);
    }
    read_json_file(&path).map(Some)
}

/// Save a task-level checkpoint. Increments the version automatically.
pub fn save_task_checkpoint(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    checkpoint: &TaskCheckpoint,
) -> anyhow::Result<()> {
    let dir = checkpoints_dir(config, &checkpoint.workflow_id);
    fs::create_dir_all(&dir)?;
    write_json_file(
        &task_checkpoint_path(config, &checkpoint.workflow_id, &checkpoint.task_id),
        checkpoint,
    )?;
    tracing::info!(
        target: "workflow",
        workflow_id = %checkpoint.workflow_id,
        task_id = %checkpoint.task_id,
        version = checkpoint.version,
        step = %checkpoint.step,
        "Saved task checkpoint"
    );
    Ok(())
}

/// Load a task checkpoint.
pub fn load_task_checkpoint(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskCheckpoint>> {
    let path = task_checkpoint_path(config, workflow_id, task_id);
    if !path.exists() {
        return Ok(None);
    }
    read_json_file(&path).map(Some)
}

/// Load all task checkpoints for a workflow.
pub fn load_all_task_checkpoints(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<TaskCheckpoint>> {
    let dir = checkpoints_dir(config, workflow_id);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if name == "planner" {
            continue; // skip planner checkpoint
        }
        match read_json_file::<TaskCheckpoint>(&path) {
            Ok(cp) => out.push(cp),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip invalid task checkpoint");
            }
        }
    }
    out.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Ok(out)
}

/// Create a new planner checkpoint from the current workflow state.
/// Auto-increments the version from the existing checkpoint.
pub fn checkpoint_planner(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    planner_intent: String,
    context: serde_json::Value,
) -> anyhow::Result<WorkflowCheckpoint> {
    let run = load_workflow_run(config, store, workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", workflow_id))?;

    let prev_version = load_workflow_checkpoint(config, store, workflow_id)?
        .map(|cp| cp.version)
        .unwrap_or(0);

    let checkpoint = WorkflowCheckpoint {
        workflow_id: workflow_id.to_string(),
        version: prev_version + 1,
        planner_intent,
        pending_task_ids: run.join_task_ids.clone(),
        join_policy: run.join_policy,
        context,
        created_at: now_rfc3339(),
    };
    save_workflow_checkpoint(config, store, &checkpoint)?;

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: None,
            event_type: "workflow.checkpoint.saved".to_string(),
            agent_id: None,
            payload: serde_json::json!({
                "version": checkpoint.version,
                "planner_intent": checkpoint.planner_intent,
                "pending_task_ids": checkpoint.pending_task_ids,
            }),
            occurred_at: now_rfc3339(),
        },
    )?;

    Ok(checkpoint)
}

/// Create a new task checkpoint. Auto-increments the version.
pub fn checkpoint_task(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    step: String,
    state: serde_json::Value,
) -> anyhow::Result<TaskCheckpoint> {
    let prev_version = load_task_checkpoint(config, store, workflow_id, task_id)?
        .map(|cp| cp.version)
        .unwrap_or(0);

    let checkpoint = TaskCheckpoint {
        workflow_id: workflow_id.to_string(),
        task_id: task_id.to_string(),
        version: prev_version + 1,
        step: step.clone(),
        state,
        created_at: now_rfc3339(),
    };
    save_task_checkpoint(config, store, &checkpoint)?;

    append_workflow_event(
        config,
        store,
        &WorkflowEventRecord {
            event_id: new_event_id(),
            workflow_id: workflow_id.to_string(),
            task_id: Some(task_id.to_string()),
            event_type: "task.checkpoint.saved".to_string(),
            agent_id: None,
            payload: serde_json::json!({
                "version": checkpoint.version,
                "step": step,
            }),
            occurred_at: now_rfc3339(),
        },
    )?;

    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::config::GatewayConfig;
    use autonoetic_types::workflow::JoinPolicy;
    use std::path::Path;
    use tempfile::tempdir;

    fn test_config(agents_dir: &Path) -> GatewayConfig {
        GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        }
    }

    /// Issue #740: `update_task_run_status` refuses illegal transitions at the
    /// single mutation choke point, returning Ok-noop and leaving the task in
    /// its current state.
    #[test]
    fn update_task_run_status_refuses_illegal_transition() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "illegal-tx-root", None).unwrap();
        let task = TaskRun {
            task_id: "task-illegal".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "a".to_string(),
            session_id: "illegal-tx-root/x".to_string(),
            parent_session_id: "illegal-tx-root".to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        // Succeeded -> Runnable is illegal; the call should be a no-op.
        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-illegal",
            TaskRunStatus::Runnable,
            None,
            None,
            None,
        )
        .unwrap();

        // Task should still be Succeeded.
        let after = load_task_run(&cfg, Some(&store), &wf.workflow_id, "task-illegal")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.status,
            TaskRunStatus::Succeeded,
            "illegal transition must leave the task in its current state"
        );
    }

    /// Issue #740: legal transitions (e.g. Stale -> Runnable for late approval)
    /// are still allowed.
    #[test]
    fn update_task_run_status_allows_stale_to_runnable() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "stale-resume-root", None).unwrap();
        let task = TaskRun {
            task_id: "task-stale".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "a".to_string(),
            session_id: "stale-resume-root/x".to_string(),
            parent_session_id: "stale-resume-root".to_string(),
            status: TaskRunStatus::Stale,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-stale",
            TaskRunStatus::Runnable,
            None,
            None,
            None,
        )
        .unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf.workflow_id, "task-stale")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, TaskRunStatus::Runnable);
    }

    #[test]
    fn scheduled_job_completed_mirrors_result_to_session_timeline() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf_id = "sched-sj-test";
        let wf_run = autonoetic_types::workflow::WorkflowRun {
            workflow_id: wf_id.to_string(),
            root_session_id: "root-cron".to_string(),
            lead_agent_id: "planner.default".to_string(),
            status: autonoetic_types::workflow::WorkflowRunStatus::Active,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            active_task_ids: vec![],
            queued_task_ids: vec![],
            join_policy: JoinPolicy::AllOf,
            join_task_ids: vec![],
            active_plan_ref: None,
            reactivated_for_root_spawn: false,
        };
        save_workflow_run(&cfg, Some(&store), &wf_run).unwrap();

        append_workflow_event(
            &cfg,
            Some(&store),
            &WorkflowEventRecord {
                event_id: "wevt-sched-test".to_string(),
                workflow_id: wf_id.to_string(),
                event_type: "task.completed".to_string(),
                task_id: Some("task-fib".to_string()),
                agent_id: Some("fibonacci-next@rev_sha256:abc".to_string()),
                payload: serde_json::json!({
                    "status": "Succeeded",
                    "result_summary": "next=21",
                }),
                occurred_at: now_rfc3339(),
            },
        )
        .unwrap();

        let tl = store
            .list_session_timeline("root-cron", None, 10, None, None)
            .unwrap();
        let completed = tl
            .entries
            .iter()
            .find(|e| e.event_type == "scheduled_job.completed")
            .expect("scheduled_job.completed must reach the timeline");
        assert!(completed
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("next=21"));
        assert!(completed
            .payload
            .as_deref()
            .unwrap_or("")
            .contains("fibonacci-next"));
    }

    /// Manual `/curate` (router.rs `curation.run_for_session`) enqueues a
    /// task whose id starts with `curate-` into the root's `wf-{root}`
    /// workflow. `maybe_emit_workflow_timeline` must admit it and map
    /// `task.completed` → `curation.completed`. Without this the operator
    /// sees nothing on the timeline after `/curate` (session-3739f831 gap).
    #[test]
    fn curation_task_completed_mirrors_result_to_session_timeline() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        // Mirror what `ensure_workflow_for_root_session` produces: a `wf-*`
        // workflow for the root, registered in RootWorkflowIndex so
        // `resolve_root_session_id` can find it.
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "root-curate", None).unwrap();

        append_workflow_event(
            &cfg,
            Some(&store),
            &WorkflowEventRecord {
                event_id: "wevt-curate-test".to_string(),
                workflow_id: wf.workflow_id.clone(),
                event_type: "task.completed".to_string(),
                // The discriminator: `curate-` prefix identifies operator-RPC
                // curation tasks (router.rs mints via short_random_id("curate-")).
                task_id: Some("curate-abcd1234".to_string()),
                agent_id: Some("memory-curator.default@rev_sha256:abc".to_string()),
                payload: serde_json::json!({
                    "status": "Succeeded",
                    "result_summary": "2 decisions, 1 promote_to_skill",
                }),
                occurred_at: now_rfc3339(),
            },
        )
        .unwrap();

        let tl = store
            .list_session_timeline("root-curate", None, 10, None, None)
            .unwrap();
        let completed = tl
            .entries
            .iter()
            .find(|e| e.event_type == "curation.completed")
            .expect("curation.completed must reach the root timeline");
        let payload_str = completed.payload.as_deref().unwrap_or("");
        assert!(
            payload_str.contains("2 decisions"),
            "payload should carry the curator's result_summary; got: {payload_str}"
        );
        assert!(
            payload_str.contains("memory-curator.default"),
            "payload should name the curator agent; got: {payload_str}"
        );
    }

    /// `task.failed` on a `curate-*` task must map to `curation.failed`
    /// (not be silently dropped, and not be misclassified as scheduled_job).
    #[test]
    fn curation_task_failed_mirrors_failure_to_session_timeline() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "root-curate-fail", None).unwrap();

        append_workflow_event(
            &cfg,
            Some(&store),
            &WorkflowEventRecord {
                event_id: "wevt-curate-fail".to_string(),
                workflow_id: wf.workflow_id.clone(),
                event_type: "task.failed".to_string(),
                task_id: Some("curate-deadbeef".to_string()),
                agent_id: Some("memory-curator.default@rev_sha256:abc".to_string()),
                payload: serde_json::json!({
                    "reason": "curator exceeded turn budget",
                }),
                occurred_at: now_rfc3339(),
            },
        )
        .unwrap();

        let tl = store
            .list_session_timeline("root-curate-fail", None, 10, None, None)
            .unwrap();
        let failed = tl
            .entries
            .iter()
            .find(|e| e.event_type == "curation.failed")
            .expect("curation.failed must reach the root timeline");
        assert!(
            failed
                .payload
                .as_deref()
                .unwrap_or("")
                .contains("curator exceeded turn budget"),
            "curation.failed payload should carry the failure reason"
        );
    }

    /// Regression guard: the broaden must NOT change `sched-*` behavior.
    /// A scheduled-job completion still maps to `scheduled_job.completed`,
    /// never to `curation.*`.
    #[test]
    fn scheduled_job_still_maps_to_scheduled_job_timeline_after_broaden() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf_id = "sched-sj-regress";
        let wf_run = autonoetic_types::workflow::WorkflowRun {
            workflow_id: wf_id.to_string(),
            root_session_id: "root-regress".to_string(),
            lead_agent_id: "planner.default".to_string(),
            status: autonoetic_types::workflow::WorkflowRunStatus::Active,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            active_task_ids: vec![],
            queued_task_ids: vec![],
            join_policy: JoinPolicy::AllOf,
            join_task_ids: vec![],
            active_plan_ref: None,
            reactivated_for_root_spawn: false,
        };
        save_workflow_run(&cfg, Some(&store), &wf_run).unwrap();

        append_workflow_event(
            &cfg,
            Some(&store),
            &WorkflowEventRecord {
                event_id: "wevt-regress".to_string(),
                workflow_id: wf_id.to_string(),
                event_type: "task.completed".to_string(),
                task_id: Some("task-cron-xyz".to_string()),
                agent_id: Some("planner.default@rev_sha256:abc".to_string()),
                payload: serde_json::json!({ "result_summary": "ok" }),
                occurred_at: now_rfc3339(),
            },
        )
        .unwrap();

        let tl = store
            .list_session_timeline("root-regress", None, 10, None, None)
            .unwrap();
        // scheduled_job.completed must be present…
        assert!(
            tl.entries
                .iter()
                .any(|e| e.event_type == "scheduled_job.completed"),
            "sched-* task.completed must still map to scheduled_job.completed"
        );
        // …and no curation.* event should leak in for a non-curate task id.
        assert!(
            !tl.entries
                .iter()
                .any(|e| e.event_type.starts_with("curation.")),
            "sched-* events must never be misclassified as curation.*"
        );
    }

    #[test]
    fn ensure_workflow_is_idempotent_per_root() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let a = ensure_workflow_for_root_session(&cfg, None, "demo-root", Some("planner.default"))
            .unwrap();
        let b = ensure_workflow_for_root_session(&cfg, None, "demo-root", Some("other")).unwrap();
        assert_eq!(a.workflow_id, b.workflow_id);
        assert_eq!(a.root_session_id, "demo-root");
        assert_eq!(b.lead_agent_id, "planner.default");
    }

    #[test]
    fn task_roundtrip_and_events_append() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "r1", None).unwrap();
        let tid = new_task_id();
        let ts = now_rfc3339();
        let task = TaskRun {
            task_id: tid.clone(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "r1/coder-abc".to_string(),
            parent_session_id: "r1".to_string(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 1,
            last_failure_class: Some(autonoetic_types::tool_error::FailureClass::TransientInfra),
            retry_policy: Some(serde_json::json!({"max_retries": 3})),
            side_effect_state: Some(autonoetic_types::tool_error::SideEffectState::Unknown),
            dedupe_key: Some("durable:coder.default:r1".to_string()),
        };
        save_task_run(&cfg, None, &task).unwrap();
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            &tid,
            TaskRunStatus::Succeeded,
            Some("ok".to_string()),
            None,
            None,
        )
        .unwrap();
        let loaded = load_task_run(&cfg, None, &wf.workflow_id, &tid)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Succeeded);
        assert_eq!(loaded.result_summary.as_deref(), Some("ok"));
        assert_eq!(loaded.retry_count, 1);
        assert_eq!(
            loaded.last_failure_class,
            Some(autonoetic_types::tool_error::FailureClass::TransientInfra)
        );
        assert_eq!(
            loaded.retry_policy,
            Some(serde_json::json!({"max_retries": 3}))
        );
        assert_eq!(
            loaded.side_effect_state,
            Some(autonoetic_types::tool_error::SideEffectState::Unknown)
        );
        assert_eq!(
            loaded.dedupe_key.as_deref(),
            Some("durable:coder.default:r1")
        );
    }

    #[test]
    fn load_task_run_backfills_missing_mechanical_fields() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "legacy-root", None).unwrap();
        let task_id = "task-legacy";

        let legacy_json = serde_json::json!({
            "task_id": task_id,
            "workflow_id": wf.workflow_id,
            "agent_id": "coder.default",
            "session_id": "legacy-root/coder-x",
            "parent_session_id": "legacy-root",
            "status": "running",
            "created_at": now_rfc3339(),
            "updated_at": now_rfc3339(),
            "source_agent_id": null,
            "result_summary": null,
            "join_group": null,
            "message": null,
            "metadata": null
        });
        let path = task_run_path(&cfg, &wf.workflow_id, task_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy_json).unwrap()).unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, task_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.retry_count, 0);
        assert!(loaded.last_failure_class.is_none());
        assert!(loaded.retry_policy.is_none());
        assert!(loaded.side_effect_state.is_none());
        assert!(loaded.dedupe_key.is_none());
    }

    /// `workflow.wait`'s own tool description tells agents that a succeeded
    /// task's `implicit_artifact_id` and `named_outputs` are how you "inspect
    /// full payload". For a script-mode agent that prints its result to stdout,
    /// both used to come back empty — the handle existed and pointed at nothing,
    /// so a planner that followed the documented path found no data and re-ran
    /// the work. Spilling the oversized reply into the child's content namespace
    /// makes that promise true.
    #[test]
    fn implicit_artifact_carries_the_spilled_child_reply() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);

        let gw_dir = crate::execution::gateway_root_dir(&cfg);
        let parent_session = "root-spill";
        let child_session = "root-spill/gmail-1";
        crate::runtime::content_store::ContentStore::new(&gw_dir)
            .unwrap()
            .set_root_session(child_session, parent_session)
            .unwrap();

        // A payload too large for the summary field, as a script agent would
        // print it: one JSON document on stdout.
        let reply = serde_json::json!({
            "ok": true,
            "emails": (0..10)
                .map(|i| serde_json::json!({
                    "id": format!("{}", 888 - i),
                    "preview": "y".repeat(400),
                }))
                .collect::<Vec<_>>(),
        })
        .to_string();

        let prepared = crate::scheduler::prepare_child_reply(
            &cfg,
            child_session,
            &reply,
            crate::scheduler::CHILD_SUMMARY_MAX_CHARS,
        );
        let spilled_ref = prepared.full_ref.clone().expect("reply must spill");

        let ts = now_rfc3339();
        let task = TaskRun {
            task_id: "task-spill-artifact".to_string(),
            workflow_id: "wf-spill-artifact".to_string(),
            agent_id: "gmail".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: parent_session.to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some("planner.default".to_string()),
            result_summary: Some(prepared.summary.clone()),
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };

        create_implicit_artifact(&cfg, None, &task, Some(&prepared.summary)).unwrap();

        let store = crate::runtime::content_store::ContentStore::new(&gw_dir).unwrap();
        let artifact_bytes = store
            .read_by_name(parent_session, &format!("impl_{}", task.task_id))
            .unwrap();
        let artifact_json: serde_json::Value = serde_json::from_slice(&artifact_bytes).unwrap();
        let named_outputs = artifact_json["content"]["named_outputs"]
            .as_array()
            .expect("named_outputs must be present");

        let entry = named_outputs
            .iter()
            .find(|o| {
                o["name"]
                    == serde_json::json!(crate::scheduler::child_reply_spill_name(
                        child_session,
                        true
                    ))
            })
            .expect("the spilled reply must be listed as a named output");
        assert_eq!(
            entry["ref"],
            serde_json::json!(spilled_ref),
            "the listed ref must be the one the parent was handed"
        );

        // The point of the whole exercise: the parent can read the full payload,
        // through the `named_outputs[*].ref` the tool description points at.
        // Ref-based, not name-based — the ref is content-addressed and is the
        // addressing that survives several children spilling under one root.
        let listed_ref = entry["ref"].as_str().unwrap();
        let recovered = String::from_utf8(
            store
                .read_by_name_or_handle(parent_session, listed_ref)
                .expect("the listed ref must resolve for the parent"),
        )
        .unwrap();
        assert_eq!(
            recovered, reply,
            "the parent must recover the untruncated reply"
        );
    }

    #[test]
    fn create_implicit_artifact_filters_outputs_not_visible_to_parent() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);

        let gw_dir = crate::execution::gateway_root_dir(&cfg);
        let store = crate::runtime::content_store::ContentStore::new(&gw_dir).unwrap();

        let parent_session = "root-visible";
        let child_session = "root-visible/researcher-abc";
        store
            .set_root_session(child_session, parent_session)
            .unwrap();

        let exported_handle = store.write(b"weather summary").unwrap();
        store
            .register_name_with_visibility(
                child_session,
                "weather.md",
                &exported_handle,
                crate::runtime::content_store::ContentVisibility::Session,
            )
            .unwrap();

        let transcript_handle = store.write(b"internal transcript").unwrap();
        store
            .register_name(child_session, "session_history", &transcript_handle)
            .unwrap();

        let ts = now_rfc3339();
        let task = TaskRun {
            task_id: "task-visible-filter".to_string(),
            workflow_id: "wf-visible-filter".to_string(),
            agent_id: "researcher.default".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: parent_session.to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };

        create_implicit_artifact(&cfg, None, &task, Some("done")).unwrap();

        let store = crate::runtime::content_store::ContentStore::new(&gw_dir).unwrap();
        let artifact_name = format!("impl_{}", task.task_id);
        let artifact_bytes = store.read_by_name(parent_session, &artifact_name).unwrap();
        let artifact_json: serde_json::Value = serde_json::from_slice(&artifact_bytes).unwrap();
        let named_outputs = artifact_json
            .get("content")
            .and_then(|content| content.get("named_outputs"))
            .and_then(|outputs| outputs.as_array())
            .unwrap();

        assert_eq!(named_outputs.len(), 1);
        assert_eq!(
            named_outputs[0]
                .get("name")
                .and_then(|value| value.as_str()),
            Some("weather.md")
        );

        let exported_ref = format!(
            "cnt_{}",
            crate::runtime::content_store::ContentStore::get_short_alias(&exported_handle)
        );
        assert_eq!(
            named_outputs[0].get("ref").and_then(|value| value.as_str()),
            Some(exported_ref.as_str())
        );
        assert!(named_outputs.iter().all(|entry| {
            entry.get("name").and_then(|value| value.as_str()) != Some("session_history")
        }));
    }

    #[test]
    fn create_implicit_artifact_includes_workflow_scoped_artifact_ref() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);

        let gw_dir = crate::execution::gateway_root_dir(&cfg);
        let gateway_store = crate::scheduler::gateway_store::GatewayStore::open(&gw_dir).unwrap();
        let content_store = crate::runtime::content_store::ContentStore::new(&gw_dir).unwrap();

        let parent_session = "root-artifact-ref";
        let child_session = "root-artifact-ref/coder-abc";
        content_store
            .set_root_session(child_session, parent_session)
            .unwrap();

        let source_handle = content_store.write(b"print('hello')").unwrap();
        content_store
            .register_name_with_visibility(
                child_session,
                "weather/main.py",
                &source_handle,
                crate::runtime::content_store::ContentVisibility::Session,
            )
            .unwrap();

        let artifact_store = crate::artifact_store::ArtifactStore::new(&gw_dir).unwrap();
        let bundle = artifact_store
            .build(
                &["weather/main.py".to_string()],
                Some(&["weather/main.py".to_string()]),
                None,
                child_session,
            )
            .unwrap();

        let workflow_id = "wf-artifact-ref";
        gateway_store
            .create_artifact_ref(&autonoetic_types::artifact::ArtifactRefRecord {
                ref_id: "ar.test123456".to_string(),
                scope_type: autonoetic_types::artifact::ArtifactRefScopeType::Workflow,
                scope_id: workflow_id.to_string(),
                artifact_id: bundle.artifact_id.clone(),
                artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
                artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
                created_by_agent_id: "coder.default".to_string(),
                created_at: now_rfc3339(),
                expires_at: None,
                revoked_at: None,
            })
            .unwrap();

        let ts = now_rfc3339();
        let task = TaskRun {
            task_id: "task-artifact-ref".to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "coder.default".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: parent_session.to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };

        create_implicit_artifact(&cfg, Some(&gateway_store), &task, Some("done")).unwrap();

        let content_store = crate::runtime::content_store::ContentStore::new(&gw_dir).unwrap();
        let artifact_name = format!("impl_{}", task.task_id);
        let artifact_bytes = content_store
            .read_by_name(parent_session, &artifact_name)
            .unwrap();
        let artifact_json: serde_json::Value = serde_json::from_slice(&artifact_bytes).unwrap();
        let artifacts = artifact_json
            .get("content")
            .and_then(|content| content.get("artifacts"))
            .and_then(|outputs| outputs.as_array())
            .unwrap();

        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            artifacts[0]
                .get("artifact_ref")
                .and_then(|value| value.as_str()),
            Some("ar.test123456")
        );
    }

    #[test]
    fn resolve_root_and_load_workflow_events() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "root-resolve", None).unwrap();
        let resolved = resolve_workflow_id_for_root_session(&cfg, "root-resolve")
            .unwrap()
            .expect("index");
        assert_eq!(resolved, wf.workflow_id);
        assert!(resolve_workflow_id_for_root_session(&cfg, "unknown-root")
            .unwrap()
            .is_none());
        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(!events.is_empty());
        assert_eq!(events[0].event_type, "workflow.started");
    }

    #[test]
    fn reroute_chat_ingest_child_session_to_root() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let mut wf =
            ensure_workflow_for_root_session(&cfg, None, "root-2b12", Some("planner.default"))
                .unwrap();
        wf.status = WorkflowRunStatus::WaitingChildren;
        let tid = "task-child-1".to_string();
        wf.join_task_ids = vec![tid.clone()];
        wf.updated_at = now_rfc3339();
        save_workflow_run(&cfg, None, &wf).unwrap();
        let ts = now_rfc3339();
        let child_session = "root-2b12/delegation-coder";
        let task = TaskRun {
            task_id: tid,
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: "root-2b12".to_string(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: Some("g1".to_string()),
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        assert!(
            reroute_chat_ingest_for_active_workflow_child_session(&cfg, None, child_session)
                .unwrap()
                .is_some_and(|r| {
                    r.root_session_id == "root-2b12"
                        && r.lead_agent_id.as_deref() == Some("planner.default")
                })
        );
        assert!(
            reroute_chat_ingest_for_active_workflow_child_session(&cfg, None, "root-2b12")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reroute_chat_ingest_skips_corrupt_workflow_json() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf =
            ensure_workflow_for_root_session(&cfg, None, "root-corrupt", Some("planner.default"))
                .unwrap();
        let corrupt_path = workflow_run_path(&cfg, &wf.workflow_id);
        std::fs::write(
            &corrupt_path,
            r#"{
  "workflow_id": "wf-bad",
  "status": "active"
} trailing garbage"#,
        )
        .unwrap();

        assert!(reroute_chat_ingest_for_active_workflow_child_session(
            &cfg,
            None,
            "root-corrupt/child-y"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn reroute_chat_ingest_skips_completed_workflow() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let mut wf =
            ensure_workflow_for_root_session(&cfg, None, "root-term", Some("planner.default"))
                .unwrap();
        wf.status = WorkflowRunStatus::Completed;
        wf.updated_at = now_rfc3339();
        save_workflow_run(&cfg, None, &wf).unwrap();
        let ts = now_rfc3339();
        let child_session = "root-term/child-x";
        let task = TaskRun {
            task_id: "t1".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: "root-term".to_string(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();
        assert!(
            reroute_chat_ingest_for_active_workflow_child_session(&cfg, None, child_session)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn list_task_runs_for_workflow_sorts_and_loads() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "root-list", None).unwrap();
        let t1 = new_task_id();
        let t2 = new_task_id();
        let ts = now_rfc3339();
        for (tid, agent) in [(&t1, "a.one"), (&t2, "a.two")] {
            let task = TaskRun {
                task_id: (*tid).clone(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: agent.to_string(),
                session_id: format!("root-list/{agent}-x"),
                parent_session_id: "root-list".to_string(),
                status: TaskRunStatus::Running,
                created_at: ts.clone(),
                updated_at: ts.clone(),
                source_agent_id: None,
                result_summary: None,
                join_group: None,
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }
        let listed = list_task_runs_for_workflow(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|t| t.task_id == t1));
        assert!(listed.iter().any(|t| t.task_id == t2));
    }

    #[test]
    fn workflow_graph_md_written_on_event_append() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf =
            ensure_workflow_for_root_session(&cfg, None, "graph-root", Some("lead.agent")).unwrap();
        let graph_path = crate::execution::gateway_root_dir(&cfg)
            .join("sessions")
            .join("graph-root")
            .join("workflow_graph.md");
        assert!(graph_path.exists());
        let text = std::fs::read_to_string(&graph_path).unwrap();
        assert!(text.contains(&wf.workflow_id));
        assert!(text.contains("graph-root"));
        assert!(text.contains("lead.agent"));
        assert!(text.contains("workflow.started") || text.contains("Recent workflow"));
    }

    // -----------------------------------------------------------------------
    // Async workflow tests (Phase 2–7)
    // -----------------------------------------------------------------------

    #[test]
    fn queue_dequeue_roundtrip() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "q-root", None).unwrap();

        let queued = QueuedTaskRun {
            task_id: "task-q1".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            message: "Write hello world".to_string(),
            child_session_id: "q-root/coder-q1".to_string(),
            parent_session_id: "q-root".to_string(),
            source_agent_id: "planner.default".to_string(),
            metadata: None,
            join_group: None,
            blocks_planner: true,
            enqueued_at: now_rfc3339(),
            credential_bindings: vec![],
        };
        enqueue_task(&cfg, None, &queued).unwrap();

        // Load queued tasks
        let loaded = load_queued_tasks(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, "task-q1");

        // Workflow should have queued_task_ids
        let run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert!(run.queued_task_ids.contains(&"task-q1".to_string()));

        // Dequeue
        dequeue_task(&cfg, None, &wf.workflow_id, "task-q1").unwrap();
        let loaded = load_queued_tasks(&cfg, None, &wf.workflow_id).unwrap();
        assert!(loaded.is_empty());
    }

    /// Emergency stop is the circuit breaker: work it refuses must not be left
    /// behind in an executable state (#883). The drain path reads the
    /// queued-tasks table, so a row committed before the refusal would run as
    /// soon as the stop lifted — while the caller, told the enqueue failed, had
    /// no reason to look for it.
    #[test]
    fn enqueue_refused_under_emergency_stop_leaves_no_queue_row() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "estop-root", None).unwrap();

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.status = WorkflowRunStatus::EmergencyStopped;
        save_workflow_run(&cfg, None, &run).unwrap();

        let queued = QueuedTaskRun {
            task_id: "task-estop".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            message: "work refused during the stop".to_string(),
            child_session_id: "estop-root/coder-estop".to_string(),
            parent_session_id: "estop-root".to_string(),
            source_agent_id: "planner.default".to_string(),
            metadata: None,
            join_group: None,
            blocks_planner: false,
            enqueued_at: now_rfc3339(),
            credential_bindings: vec![],
        };
        let err = enqueue_task(&cfg, None, &queued).expect_err("enqueue must be refused");
        assert!(
            err.to_string().contains("emergency stop"),
            "unexpected error: {err}"
        );

        // Nothing queued, and nothing registered on the run.
        assert!(
            load_queued_tasks(&cfg, None, &wf.workflow_id)
                .unwrap()
                .is_empty(),
            "refused work must leave no drainable row"
        );
        let run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert!(!run.queued_task_ids.contains(&"task-estop".to_string()));

        // Lifting the stop must not resurrect it either — the task is simply gone.
        let mut run = run;
        run.status = WorkflowRunStatus::Active;
        save_workflow_run(&cfg, None, &run).unwrap();
        assert!(load_queued_tasks(&cfg, None, &wf.workflow_id)
            .unwrap()
            .is_empty());
    }

    /// The two views of a queued task — the queue table the drain reads and the
    /// run record's `queued_task_ids` — must agree after a successful enqueue.
    #[test]
    fn successful_enqueue_registers_the_task_in_both_views() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "both-views-root", None).unwrap();

        let queued = QueuedTaskRun {
            task_id: "task-both".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            message: "ok".to_string(),
            child_session_id: "both-views-root/coder".to_string(),
            parent_session_id: "both-views-root".to_string(),
            source_agent_id: "planner.default".to_string(),
            metadata: None,
            join_group: None,
            blocks_planner: false,
            enqueued_at: now_rfc3339(),
            credential_bindings: vec![],
        };
        enqueue_task(&cfg, None, &queued).unwrap();

        let rows = load_queued_tasks(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, "task-both");
        let run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert!(run.queued_task_ids.contains(&"task-both".to_string()));
    }

    #[test]
    fn parallel_async_enqueue_and_join_condition() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "parallel-root", None).unwrap();

        // Enqueue two tasks in the same join group
        for (tid, agent) in [
            ("task-p1", "coder.default"),
            ("task-p2", "researcher.default"),
        ] {
            let queued = QueuedTaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: agent.to_string(),
                message: format!("Do {}", tid),
                child_session_id: format!("parallel-root/{}-x", agent),
                parent_session_id: "parallel-root".to_string(),
                source_agent_id: "planner.default".to_string(),
                metadata: None,
                join_group: Some("main".to_string()),
                blocks_planner: true,
                enqueued_at: now_rfc3339(),
                credential_bindings: vec![],
            };
            enqueue_task(&cfg, None, &queued).unwrap();
        }

        // Both should be in queue
        let queued = load_queued_tasks(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(queued.len(), 2);

        // Load all queued tasks across all workflows
        let all_queued = load_all_queued_tasks(&cfg, None).unwrap();
        assert!(all_queued.len() >= 2);

        // Dequeue both and create TaskRuns
        for tid in ["task-p1", "task-p2"] {
            dequeue_task(&cfg, None, &wf.workflow_id, tid).unwrap();
            let task = TaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "coder.default".to_string(),
                session_id: format!("parallel-root/{}-x", tid),
                parent_session_id: "parallel-root".to_string(),
                status: TaskRunStatus::Running,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: Some("planner.default".to_string()),
                result_summary: None,
                join_group: Some("main".to_string()),
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        // Check join: both still Running → not satisfied
        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-p1".to_string(), "task-p2".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();
        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Complete first task
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-p1",
            TaskRunStatus::Succeeded,
            Some("done p1".to_string()),
            None,
            None,
        )
        .unwrap();

        // Join still not satisfied (task-p2 still running)
        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Complete second task
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-p2",
            TaskRunStatus::Succeeded,
            Some("done p2".to_string()),
            None,
            None,
        )
        .unwrap();

        // Now join should be satisfied
        assert!(check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Workflow should be Resumable
        let run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Resumable);
    }

    #[test]
    fn join_satisfied_emits_event() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "join-ev-root", None).unwrap();

        // Create task and set join condition
        let task = TaskRun {
            task_id: "task-je1".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "a".to_string(),
            session_id: "join-ev-root/a-x".to_string(),
            parent_session_id: "join-ev-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-je1".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        // Complete the task → should emit workflow.join.satisfied
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-je1",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "workflow.join.satisfied"),
            "Expected workflow.join.satisfied event, got: {:?}",
            events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn join_treats_stale_as_terminal() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "join-stale-root", None).unwrap();

        for (task_id, status) in [
            ("task-ok", TaskRunStatus::AwaitingApproval),
            ("task-stale", TaskRunStatus::AwaitingApproval),
        ] {
            let task = TaskRun {
                task_id: task_id.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                session_id: format!("join-stale-root/{task_id}-x"),
                parent_session_id: "join-stale-root".to_string(),
                status,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: None,
                result_summary: None,
                join_group: Some("main".to_string()),
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-ok".to_string(), "task-stale".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-ok",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-stale",
            TaskRunStatus::Stale,
            None,
            None,
            None,
        )
        .unwrap();

        assert!(
            check_join_condition(&cfg, None, &wf.workflow_id).unwrap(),
            "Stale join task should be treated as terminal"
        );
    }

    #[test]
    fn join_satisfied_event_is_not_re_emitted_once_workflow_is_resumable() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "join-once-root", None).unwrap();

        for (task_id, session_id) in [
            ("task-join", "join-once-root/a-x"),
            ("task-extra", "join-once-root/b-x"),
        ] {
            let task = TaskRun {
                task_id: task_id.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                session_id: session_id.to_string(),
                parent_session_id: "join-once-root".to_string(),
                status: TaskRunStatus::Running,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: None,
                result_summary: None,
                join_group: None,
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-join".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-join",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-extra",
            TaskRunStatus::Failed,
            Some("late failure".to_string()),
            None,
            None,
        )
        .unwrap();

        let join_events = load_workflow_events(&cfg, None, &wf.workflow_id)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == "workflow.join.satisfied")
            .count();
        assert_eq!(
            join_events, 1,
            "workflow.join.satisfied should only be emitted once"
        );
    }

    #[test]
    fn failed_task_still_satisfies_join() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "fail-join-root", None).unwrap();

        for tid in ["task-f1", "task-f2"] {
            let task = TaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                session_id: format!("fail-join-root/{}-x", tid),
                parent_session_id: "fail-join-root".to_string(),
                status: TaskRunStatus::Running,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: None,
                result_summary: None,
                join_group: None,
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-f1".to_string(), "task-f2".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        // Task 1 fails
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-f1",
            TaskRunStatus::Failed,
            Some("error".to_string()),
            None,
            None,
        )
        .unwrap();
        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Task 2 succeeds → join satisfied even though one failed
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-f2",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        let run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Resumable);
    }

    #[test]
    fn compact_workflow_summary_none_when_no_tasks() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let _wf = ensure_workflow_for_root_session(&cfg, None, "summary-root", None).unwrap();

        let summary = compact_workflow_summary(&cfg, None, "summary-root").unwrap();
        assert!(summary.is_none());
    }

    #[test]
    fn compact_workflow_summary_counts_tasks() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "sum-root", None).unwrap();

        // 1 running, 1 succeeded, 1 queued
        let running = TaskRun {
            task_id: "t-run".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "a".to_string(),
            session_id: "sum-root/a-x".to_string(),
            parent_session_id: "sum-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &running).unwrap();

        let done = TaskRun {
            task_id: "t-done".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "b".to_string(),
            session_id: "sum-root/b-x".to_string(),
            parent_session_id: "sum-root".to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: Some("ok".to_string()),
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &done).unwrap();

        let queued = QueuedTaskRun {
            task_id: "t-queued".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "c".to_string(),
            message: "todo".to_string(),
            child_session_id: "sum-root/c-x".to_string(),
            parent_session_id: "sum-root".to_string(),
            source_agent_id: "planner".to_string(),
            metadata: None,
            join_group: None,
            blocks_planner: true,
            enqueued_at: now_rfc3339(),
            credential_bindings: vec![],
        };
        enqueue_task(&cfg, None, &queued).unwrap();

        let summary = compact_workflow_summary(&cfg, None, "sum-root")
            .unwrap()
            .unwrap();
        assert!(summary.contains("1 running"), "got: {}", summary);
        assert!(summary.contains("1 queued"), "got: {}", summary);
        assert!(summary.contains("1 done"), "got: {}", summary);
    }

    #[test]
    fn approval_unblocks_task() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "appr-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-appr".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "appr-root/coder-x".to_string(),
            parent_session_id: "appr-root".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        // Simulate approval unblock (Runnable on approve)
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-appr",
            TaskRunStatus::Runnable,
            Some("approval_approved".to_string()),
            None,
            None,
        )
        .unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-appr")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Runnable);
        assert_eq!(loaded.result_summary.as_deref(), Some("approval_approved"));

        // Events should contain task.updated (Runnable maps to task.updated event)
        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.updated"));
    }

    #[test]
    fn approval_resume_preserves_message_and_metadata() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "resume-root", None).unwrap();

        let original_message = "Build a REST API with authentication".to_string();
        let original_metadata = serde_json::json!({
            "priority": "high",
            "context": "user_requested_feature"
        });

        // Create task with message and metadata (as would happen on async spawn)
        let task = TaskRun {
            task_id: "task-resume".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "resume-root/coder-x".to_string(),
            parent_session_id: "resume-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some(original_message.clone()),
            metadata: Some(original_metadata.clone()),
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        // Task hits approval barrier
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-resume",
            TaskRunStatus::AwaitingApproval,
            Some("awaiting_approval".to_string()),
            None,
            None,
        )
        .unwrap();

        // Verify message and metadata preserved through AwaitingApproval
        let awaiting = load_task_run(&cfg, None, &wf.workflow_id, "task-resume")
            .unwrap()
            .unwrap();
        assert_eq!(awaiting.status, TaskRunStatus::AwaitingApproval);
        assert_eq!(awaiting.message.as_deref(), Some(original_message.as_str()));
        assert_eq!(awaiting.metadata.as_ref(), Some(&original_metadata));

        // Simulate approval unblock
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-resume",
            TaskRunStatus::Runnable,
            Some("approval_approved".to_string()),
            None,
            None,
        )
        .unwrap();

        // Verify message and metadata still preserved for resume
        let resumed = load_task_run(&cfg, None, &wf.workflow_id, "task-resume")
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, TaskRunStatus::Runnable);
        assert_eq!(resumed.message.as_deref(), Some(original_message.as_str()));
        assert_eq!(resumed.metadata.as_ref(), Some(&original_metadata));
    }

    #[test]
    fn join_group_any_satisfies() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "jg-root", None).unwrap();

        // Two tasks in DIFFERENT groups
        for (tid, grp) in [("t-r1", "research"), ("t-c1", "coding")] {
            let task = TaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                session_id: format!("jg-root/{}-x", tid),
                parent_session_id: "jg-root".to_string(),
                status: TaskRunStatus::Running,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: None,
                result_summary: None,
                join_group: Some(grp.to_string()),
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["t-r1".to_string(), "t-c1".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        // Neither group satisfied yet
        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Complete research group task — join satisfied (ANY group)
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "t-r1",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Coding task still running — that's fine, research group is done
        let coding = load_task_run(&cfg, None, &wf.workflow_id, "t-c1")
            .unwrap()
            .unwrap();
        assert_eq!(coding.status, TaskRunStatus::Running);
    }

    #[test]
    fn join_group_same_group_needs_all() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "jgs-root", None).unwrap();

        // Two tasks in the SAME group
        for tid in ["t-a", "t-b"] {
            let task = TaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                session_id: format!("jgs-root/{}-x", tid),
                parent_session_id: "jgs-root".to_string(),
                status: TaskRunStatus::Running,
                created_at: now_rfc3339(),
                updated_at: now_rfc3339(),
                source_agent_id: None,
                result_summary: None,
                join_group: Some("group1".to_string()),
                message: None,
                metadata: None,
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: None,
            };
            save_task_run(&cfg, None, &task).unwrap();
        }

        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["t-a".to_string(), "t-b".to_string()];
        save_workflow_run(&cfg, None, &run).unwrap();

        // Complete only one — NOT satisfied (same group needs all)
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "t-a",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!check_join_condition(&cfg, None, &wf.workflow_id).unwrap());

        // Complete second — now satisfied
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "t-b",
            TaskRunStatus::Succeeded,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(check_join_condition(&cfg, None, &wf.workflow_id).unwrap());
    }

    #[test]
    fn approval_reject_fails_task() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "rej-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-rej".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "rej-root/coder-x".to_string(),
            parent_session_id: "rej-root".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        // Simulate rejection
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-rej",
            TaskRunStatus::Failed,
            Some("approval_rejected".to_string()),
            None,
            None,
        )
        .unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-rej")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Failed);
        assert_eq!(
            loaded.last_failure_class,
            Some(autonoetic_types::tool_error::FailureClass::PolicyDenied)
        );

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.failed"));
        let failed = events
            .iter()
            .find(|e| e.event_type == "task.failed")
            .unwrap();
        assert_eq!(failed.payload["failure_class"], "policy_denied");
        assert_eq!(failed.payload["retry_advice"], "do_not_retry");
    }

    #[test]
    fn awaiting_approval_event_carries_mechanical_classification() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "awaiting-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-await".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "awaiting-root/coder-x".to_string(),
            parent_session_id: "awaiting-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-await",
            TaskRunStatus::AwaitingApproval,
            None,
            Some(ApprovalMetadata {
                request_id: "apr-await123".to_string(),
                kind: "sandbox".to_string(),
                reason: Some("operator approval required".to_string()),
            }),
            None,
        )
        .unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-await")
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.last_failure_class,
            Some(autonoetic_types::tool_error::FailureClass::ApprovalPending)
        );
        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let awaiting = events
            .iter()
            .find(|e| e.event_type == "task.awaiting_approval")
            .unwrap();
        assert_eq!(awaiting.payload["failure_class"], "approval_pending");
        assert_eq!(awaiting.payload["retry_advice"], "wait");
        assert_eq!(awaiting.payload["requires_external_event"], true);
    }

    #[test]
    fn retry_policy_normalizes_transient_infra_failure_to_retry_same_stage() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "retry-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-retry".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "retry-root/coder-x".to_string(),
            parent_session_id: "retry-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: Some(serde_json::json!({
                "retry_policy": {
                    "transient_infra": { "max_retries": 1 }
                }
            })),
            retry_count: 0,
            last_failure_class: None,
            retry_policy: Some(serde_json::json!({
                "transient_infra": { "max_retries": 1 }
            })),
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-retry",
            TaskRunStatus::Failed,
            Some("connection refused while contacting registry".to_string()),
            None,
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let failed = events
            .iter()
            .find(|e| e.event_type == "task.failed")
            .unwrap();
        assert_eq!(failed.payload["failure_class"], "transient_infra");
        assert_eq!(failed.payload["retry_advice"], "retry_same_stage");
        assert!(!events
            .iter()
            .any(|e| e.event_type == "workflow.stage_budget_exhausted"));
    }

    #[test]
    fn exhausted_retry_budget_emits_stage_budget_exhausted() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf =
            ensure_workflow_for_root_session(&cfg, None, "retry-exhausted-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-retry-exhausted".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "retry-exhausted-root/coder-x".to_string(),
            parent_session_id: "retry-exhausted-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 1,
            last_failure_class: None,
            retry_policy: Some(serde_json::json!({
                "transient_infra": { "max_retries": 1 }
            })),
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            "task-retry-exhausted",
            TaskRunStatus::Failed,
            Some("connection refused while contacting registry".to_string()),
            None,
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let failed = events
            .iter()
            .find(|e| e.event_type == "task.failed")
            .unwrap();
        assert_eq!(failed.payload["retry_advice"], "do_not_retry");
        let exhausted = events
            .iter()
            .find(|e| e.event_type == "workflow.stage_budget_exhausted")
            .unwrap();
        assert_eq!(exhausted.payload["failure_class"], "transient_infra");
        assert_eq!(exhausted.payload["retry_count"], 1);
        assert_eq!(exhausted.payload["max_retries"], 1);
    }

    #[test]
    fn schedule_task_stage_retry_marks_task_runnable_without_terminal_event() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "retry-schedule-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-retry-schedule".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "retry-schedule-root/coder-x".to_string(),
            parent_session_id: "retry-schedule-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: Some("Do the work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: Some(serde_json::json!({
                "transient_infra": { "max_retries": 1 }
            })),
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        let decision = evaluate_stage_retry(
            &task,
            TaskRunStatus::Failed,
            Some("connection refused while contacting registry"),
        );
        assert!(decision.retry_scheduled);

        schedule_task_stage_retry(
            &cfg,
            None,
            &wf.workflow_id,
            "task-retry-schedule",
            Some("connection refused while contacting registry".to_string()),
            &decision,
        )
        .unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-retry-schedule")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Runnable);
        assert_eq!(loaded.retry_count, 1);
        assert_eq!(
            loaded.last_failure_class,
            Some(autonoetic_types::tool_error::FailureClass::TransientInfra)
        );

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let updated = events
            .iter()
            .find(|e| e.event_type == "task.updated")
            .expect("task.updated event should exist");
        assert_eq!(updated.payload["status"], "runnable");
        assert_eq!(updated.payload["retry_advice"], "retry_same_stage");
        assert!(
            !events.iter().any(|e| e.event_type == "task.failed"),
            "retry scheduling should not emit a terminal failure event"
        );
    }

    #[test]
    fn retry_workflow_task_moves_failed_to_runnable_and_increments_count() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "op-retry-root", None).unwrap();

        let mut task = TaskRun {
            task_id: "task-op-retry".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "op-retry-root/coder-x".to_string(),
            parent_session_id: "op-retry-root".to_string(),
            status: TaskRunStatus::Failed,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: Some("child crashed: spawn_execute_error".to_string()),
            join_group: None,
            message: Some("Do the work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: Some(
                autonoetic_types::tool_error::FailureClass::Unknown,
            ),
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        let retried = retry_workflow_task(
            &cfg,
            None,
            &wf.workflow_id,
            "task-op-retry",
            Some("operator: transient LLM 500, retrying"),
        )
        .unwrap();

        assert_eq!(retried.status, TaskRunStatus::Runnable);
        assert_eq!(retried.retry_count, 1);
        assert_eq!(
            retried.result_summary.as_deref(),
            Some("operator: transient LLM 500, retrying")
        );
        assert_eq!(retried.last_failure_class, None, "prior failure class cleared");

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-op-retry")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Runnable);
        assert_eq!(loaded.retry_count, 1);

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let updated = events
            .iter()
            .filter(|e| e.event_type == "task.updated")
            .last()
            .expect("task.updated event should exist");
        assert_eq!(updated.payload["status"], "runnable");
        assert_eq!(updated.payload["reason"], "operator_retry");
        assert_eq!(updated.payload["retry_count"], 1);
    }

    #[test]
    fn retry_workflow_task_reactivates_terminal_workflow_run() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "op-retry-term-root", None).unwrap();

        // Force the workflow run itself into a terminal state.
        let mut run =
            load_workflow_run(&cfg, None, &wf.workflow_id).unwrap().unwrap();
        run.status = WorkflowRunStatus::Failed;
        save_workflow_run(&cfg, None, &run).unwrap();
        assert!(is_workflow_terminal(&cfg, None, &wf.workflow_id).unwrap());

        let task = TaskRun {
            task_id: "task-term-retry".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "op-retry-term-root/coder-x".to_string(),
            parent_session_id: "op-retry-term-root".to_string(),
            status: TaskRunStatus::Failed,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: Some("Do the work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        retry_workflow_task(&cfg, None, &wf.workflow_id, "task-term-retry", None).unwrap();

        let revived = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            revived.status,
            WorkflowRunStatus::Resumable,
            "terminal workflow should be reactivated to Resumable"
        );
        assert!(
            revived.reactivated_for_root_spawn,
            "reactivated_for_root_spawn flag should be set"
        );
    }

    #[test]
    fn retry_workflow_task_rejects_non_terminal_task() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf =
            ensure_workflow_for_root_session(&cfg, None, "op-retry-succ-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-succeeded".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "op-retry-succ-root/coder-x".to_string(),
            parent_session_id: "op-retry-succ-root".to_string(),
            status: TaskRunStatus::Succeeded,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: Some("done".to_string()),
            join_group: None,
            message: Some("Do the work".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, None, &task).unwrap();

        let err = retry_workflow_task(&cfg, None, &wf.workflow_id, "task-succeeded", None)
            .unwrap_err();
        assert!(
            err.to_string().contains("only Failed or Aborted"),
            "non-terminal task should be rejected; got: {err}"
        );
    }

    #[test]
    fn resolve_workflow_id_for_operator_retry_normalizes_and_prefers_explicit() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "resolve-root", None).unwrap();

        // Explicit workflow_id wins (and trims surrounding whitespace).
        let resolved = resolve_workflow_id_for_operator_retry(
            &cfg,
            Some(&format!("  {}  ", wf.workflow_id)),
            Some("resolve-root"),
        )
        .unwrap();
        assert_eq!(resolved, wf.workflow_id);

        // root_session resolves when workflow_id is absent/whitespace.
        let resolved_by_root = resolve_workflow_id_for_operator_retry(
            &cfg,
            Some("   "),
            Some("resolve-root"),
        )
        .unwrap();
        assert_eq!(resolved_by_root, wf.workflow_id);

        // Neither provided (both whitespace/None) → error.
        let err = resolve_workflow_id_for_operator_retry(&cfg, Some("  "), None).unwrap_err();
        assert!(
            err.to_string().contains("either workflow_id or root_session"),
            "missing both should error; got: {err}"
        );
    }

    #[test]
    fn absent_retry_policy_does_not_schedule_retry() {
        let task = TaskRun {
            task_id: "task-no-policy".to_string(),
            workflow_id: "wf-no-policy".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-x".to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };

        let decision = evaluate_stage_retry(
            &task,
            TaskRunStatus::Failed,
            Some("connection refused while contacting registry"),
        );
        assert!(!decision.retry_scheduled);
        assert!(!decision.budget_exhausted);
        assert_eq!(
            decision.failure.and_then(|failure| failure.retry_advice),
            None,
            "absent explicit policy must not auto-schedule a retry"
        );
    }

    #[test]
    fn install_conflict_stays_non_retryable_by_default() {
        let task = TaskRun {
            task_id: "task-install-conflict".to_string(),
            workflow_id: "wf-install-conflict".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-x".to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };

        let decision = evaluate_stage_retry(
            &task,
            TaskRunStatus::Failed,
            Some("active revision exists for install target"),
        );
        assert!(!decision.retry_scheduled);
        let failure = decision.failure.expect("failure metadata");
        assert_eq!(
            failure.failure_class,
            Some(autonoetic_types::tool_error::FailureClass::InstallConflict)
        );
        assert_eq!(
            failure.retry_advice,
            Some(autonoetic_types::tool_error::RetryAdvice::DoNotRetry)
        );
    }

    #[test]
    fn timeout_with_policy_escalates_when_side_effect_state_is_unknown() {
        let task = TaskRun {
            task_id: "task-timeout".to_string(),
            workflow_id: "wf-timeout".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-x".to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: Some(serde_json::json!({
                "timeout": { "max_retries": 1 }
            })),
            side_effect_state: None,
            dedupe_key: None,
        };

        let decision =
            evaluate_stage_retry(&task, TaskRunStatus::Failed, Some("request timed out"));
        assert!(!decision.retry_scheduled);
        let failure = decision.failure.expect("failure metadata");
        assert_eq!(
            failure.failure_class,
            Some(autonoetic_types::tool_error::FailureClass::Timeout)
        );
        assert_eq!(
            failure.retry_advice,
            Some(autonoetic_types::tool_error::RetryAdvice::EscalateHuman)
        );
    }

    #[test]
    fn persisted_failure_class_outranks_prose_summary_for_retry_decisions() {
        let task = TaskRun {
            task_id: "task-structured-failure".to_string(),
            workflow_id: "wf-structured-failure".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-x".to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: Some(autonoetic_types::tool_error::FailureClass::TransientInfra),
            retry_policy: Some(serde_json::json!({
                "transient_infra": { "max_retries": 1 }
            })),
            side_effect_state: Some(autonoetic_types::tool_error::SideEffectState::NoSideEffect),
            dedupe_key: None,
        };

        let decision = evaluate_stage_retry(
            &task,
            TaskRunStatus::Failed,
            Some("child agent failed; see task metadata"),
        );
        assert!(decision.retry_scheduled);
        let failure = decision.failure.expect("failure metadata");
        assert_eq!(
            failure.failure_class,
            Some(autonoetic_types::tool_error::FailureClass::TransientInfra)
        );
        assert_eq!(
            failure.retry_advice,
            Some(autonoetic_types::tool_error::RetryAdvice::RetrySameStage)
        );
        assert_eq!(
            failure.side_effect_state,
            Some(autonoetic_types::tool_error::SideEffectState::NoSideEffect)
        );
    }

    #[test]
    fn load_all_queued_tasks_across_workflows() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);

        let wf1 = ensure_workflow_for_root_session(&cfg, None, "multi-root-1", None).unwrap();
        let wf2 = ensure_workflow_for_root_session(&cfg, None, "multi-root-2", None).unwrap();

        for (wf, tid) in [(&wf1, "t-m1"), (&wf2, "t-m2")] {
            let queued = QueuedTaskRun {
                task_id: tid.to_string(),
                workflow_id: wf.workflow_id.clone(),
                agent_id: "a".to_string(),
                message: "do".to_string(),
                child_session_id: "s".to_string(),
                parent_session_id: "p".to_string(),
                source_agent_id: "planner".to_string(),
                metadata: None,
                join_group: None,
                blocks_planner: false,
                enqueued_at: now_rfc3339(),
                credential_bindings: vec![],
            };
            enqueue_task(&cfg, None, &queued).unwrap();
        }

        let all = load_all_queued_tasks(&cfg, None).unwrap();
        let ids: Vec<&str> = all.iter().map(|q| q.task_id.as_str()).collect();
        assert!(ids.contains(&"t-m1"));
        assert!(ids.contains(&"t-m2"));
    }

    #[test]
    fn task_claim_roundtrip_and_release() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "claim-root", None).unwrap();

        let claim = acquire_task_claim(&cfg, None, &wf.workflow_id, "task-c1", 60)
            .unwrap()
            .expect("claim acquired");
        assert_eq!(claim.task_id, "task-c1");

        let fresh_claim = acquire_task_claim(&cfg, None, &wf.workflow_id, "task-c1", 60).unwrap();
        assert!(
            fresh_claim.is_none(),
            "fresh claim should block duplicate claim"
        );

        let loaded = load_task_claim(&cfg, None, &wf.workflow_id, "task-c1")
            .unwrap()
            .expect("claim present");
        assert_eq!(loaded.scheduler_instance_id, claim.scheduler_instance_id);

        release_task_claim(&cfg, None, &wf.workflow_id, "task-c1").unwrap();
        assert!(load_task_claim(&cfg, None, &wf.workflow_id, "task-c1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn stale_task_claim_can_be_reacquired() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "stale-claim-root", None).unwrap();

        let stale_claim = TaskExecutionClaim {
            workflow_id: wf.workflow_id.clone(),
            task_id: "task-stale".to_string(),
            scheduler_instance_id: "stale-instance".to_string(),
            claimed_at: now_rfc3339(),
            heartbeat_at: (Utc::now() - Duration::seconds(120)).to_rfc3339(),
        };
        write_json_file(
            &task_claim_path(&cfg, &wf.workflow_id, "task-stale"),
            &stale_claim,
        )
        .unwrap();

        let reacquired = acquire_task_claim(&cfg, None, &wf.workflow_id, "task-stale", 30)
            .unwrap()
            .expect("stale claim reacquired");
        assert_ne!(
            reacquired.scheduler_instance_id,
            stale_claim.scheduler_instance_id
        );
    }

    #[test]
    fn load_queued_tasks_skips_claim_sidecars() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "claim-sidecar-root", None).unwrap();

        let queued = QueuedTaskRun {
            task_id: "task-qclaim".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            message: "do work".to_string(),
            child_session_id: "claim-sidecar-root/coder-x".to_string(),
            parent_session_id: "claim-sidecar-root".to_string(),
            source_agent_id: "planner.default".to_string(),
            metadata: None,
            join_group: None,
            blocks_planner: true,
            enqueued_at: now_rfc3339(),
            credential_bindings: vec![],
        };
        enqueue_task(&cfg, None, &queued).unwrap();
        acquire_task_claim(&cfg, None, &wf.workflow_id, &queued.task_id, 60)
            .unwrap()
            .expect("claim acquired");

        let loaded = load_queued_tasks(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].task_id, queued.task_id);
    }

    // -----------------------------------------------------------------------
    // Checkpoint tests (Phase 3)
    // -----------------------------------------------------------------------

    #[test]
    fn planner_checkpoint_roundtrip() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "ckpt-root", None).unwrap();

        // No checkpoint initially
        assert!(load_workflow_checkpoint(&cfg, None, &wf.workflow_id)
            .unwrap()
            .is_none());

        // Create checkpoint
        let cp = checkpoint_planner(
            &cfg,
            None,
            &wf.workflow_id,
            "Waiting for coder and researcher results".to_string(),
            serde_json::json!({"delegation": "parallel analysis"}),
        )
        .unwrap();
        assert_eq!(cp.version, 1);
        assert_eq!(
            cp.planner_intent,
            "Waiting for coder and researcher results"
        );

        // Load it back
        let loaded = load_workflow_checkpoint(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.workflow_id, wf.workflow_id);

        // Second checkpoint auto-increments version
        let cp2 = checkpoint_planner(
            &cfg,
            None,
            &wf.workflow_id,
            "Processing results".to_string(),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(cp2.version, 2);
    }

    #[test]
    fn task_checkpoint_roundtrip() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "ckpt-task-root", None).unwrap();

        // No checkpoint initially
        assert!(
            load_task_checkpoint(&cfg, None, &wf.workflow_id, "task-ck1")
                .unwrap()
                .is_none()
        );

        // Create task checkpoint
        let cp = checkpoint_task(
            &cfg,
            None,
            &wf.workflow_id,
            "task-ck1",
            "writing_code".to_string(),
            serde_json::json!({"files_written": ["main.py", "utils.py"]}),
        )
        .unwrap();
        assert_eq!(cp.version, 1);
        assert_eq!(cp.step, "writing_code");

        // Load it back
        let loaded = load_task_checkpoint(&cfg, None, &wf.workflow_id, "task-ck1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.version, 1);

        // Second checkpoint auto-increments
        let cp2 = checkpoint_task(
            &cfg,
            None,
            &wf.workflow_id,
            "task-ck1",
            "running_tests".to_string(),
            serde_json::json!({"tests_run": 5}),
        )
        .unwrap();
        assert_eq!(cp2.version, 2);

        // Load all task checkpoints
        checkpoint_task(
            &cfg,
            None,
            &wf.workflow_id,
            "task-ck2",
            "setup".to_string(),
            serde_json::json!({}),
        )
        .unwrap();
        let all = load_all_task_checkpoints(&cfg, None, &wf.workflow_id).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].task_id, "task-ck1");
        assert_eq!(all[1].task_id, "task-ck2");
    }

    #[test]
    fn checkpoint_planner_captures_join_state() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "ckpt-join-root", None).unwrap();

        // Set up join task IDs
        let mut run = load_workflow_run(&cfg, None, &wf.workflow_id)
            .unwrap()
            .unwrap();
        run.join_task_ids = vec!["task-a".to_string(), "task-b".to_string()];
        run.join_policy = JoinPolicy::AllOf;
        save_workflow_run(&cfg, None, &run).unwrap();

        // Checkpoint should capture the join state
        let cp = checkpoint_planner(
            &cfg,
            None,
            &wf.workflow_id,
            "Delegated research and coding".to_string(),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(cp.pending_task_ids, vec!["task-a", "task-b"]);
        assert_eq!(cp.join_policy, JoinPolicy::AllOf);
    }

    #[test]
    fn checkpoint_events_emitted() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "ckpt-ev-root", None).unwrap();

        // Clear initial events
        let initial_count = load_workflow_events(&cfg, None, &wf.workflow_id)
            .unwrap()
            .len();

        checkpoint_planner(
            &cfg,
            None,
            &wf.workflow_id,
            "test".to_string(),
            serde_json::json!({}),
        )
        .unwrap();
        checkpoint_task(
            &cfg,
            None,
            &wf.workflow_id,
            "t1",
            "step1".to_string(),
            serde_json::json!({}),
        )
        .unwrap();

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        let new_events = &events[initial_count..];
        assert!(new_events
            .iter()
            .any(|e| e.event_type == "workflow.checkpoint.saved"));
        assert!(new_events
            .iter()
            .any(|e| e.event_type == "task.checkpoint.saved"));
    }

    // -----------------------------------------------------------------------
    // SQLite workflow event tests
    // -----------------------------------------------------------------------

    #[test]
    fn load_events_reads_from_sqlite() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "sqlite-root", None).unwrap();

        // Events from ensure_workflow_for_root_session are written to SQLite.
        let events = load_workflow_events(&cfg, Some(&store), &wf.workflow_id).unwrap();
        assert!(!events.is_empty());
        assert_eq!(events[0].event_type, "workflow.started");

        // Append through normal path
        append_workflow_event(
            &cfg,
            Some(&store),
            &WorkflowEventRecord {
                event_id: new_event_id(),
                workflow_id: wf.workflow_id.clone(),
                task_id: None,
                event_type: "test.event".to_string(),
                agent_id: None,
                payload: serde_json::json!({}),
                occurred_at: now_rfc3339(),
            },
        )
        .unwrap();

        let events = load_workflow_events(&cfg, Some(&store), &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "test.event"));
    }

    #[test]
    fn load_events_without_explicit_store_opens_gateway_db() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "implicit-store-root", None)
            .unwrap();

        // Without explicit store arg, loader opens GatewayStore from config.
        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(!events.is_empty());
        assert_eq!(events[0].event_type, "workflow.started");
    }

    #[test]
    fn approval_resume_emits_visible_event() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "resume-vis-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-resume-vis".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "resume-vis-root/coder-x".to_string(),
            parent_session_id: "resume-vis-root".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        // Simulate approval → Runnable transition
        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-resume-vis",
            TaskRunStatus::Runnable,
            Some("approval_approved".to_string()),
            None,
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, Some(&store), &wf.workflow_id).unwrap();

        // Should have task.updated event with runnable status
        let resume_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "task.updated")
            .collect();
        assert!(
            !resume_events.is_empty(),
            "Expected task.updated event for Runnable transition, got: {:?}",
            events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
        );

        // Verify the event has runnable status in payload
        let resume_event = resume_events.last().unwrap();
        let status = resume_event.payload.get("status").and_then(|v| v.as_str());
        assert_eq!(
            status,
            Some("runnable"),
            "Expected runnable status in event payload"
        );

        // Verify event is readable when store is opened implicitly.
        let events_from_implicit_store = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(
            events_from_implicit_store
                .iter()
                .any(|e| e.event_type == "task.updated"),
            "task.updated event should be in SQLite"
        );
    }

    #[test]
    fn awaiting_approval_emits_child_waiting_event_and_notification() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "child-wait-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-child-wait".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "child-wait-root/coder-x".to_string(),
            parent_session_id: "child-wait-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-child-wait",
            TaskRunStatus::AwaitingApproval,
            Some("awaiting approval apr-1".to_string()),
            Some(ApprovalMetadata {
                request_id: "apr-1".to_string(),
                kind: "sandbox".to_string(),
                reason: Some("operator approval required".to_string()),
            }),
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, Some(&store), &wf.workflow_id).unwrap();
        let waiting = events
            .iter()
            .find(|event| event.event_type == "workflow.child.waiting")
            .expect("workflow.child.waiting event should exist");
        assert_eq!(waiting.payload["task_id"], "task-child-wait");
        assert_eq!(waiting.payload["child_status"], "awaiting_approval");
        assert_eq!(waiting.payload["failure_class"], "approval_pending");

        let notifications = store
            .list_notifications_for_session(
                &wf.root_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let child_signal = notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::ChildStateNotification
            })
            .expect("child-state notification should be queued");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(child_signal.payload.clone())
                .expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::ChildStateNotification { notification, .. } => {
                assert_eq!(notification.task_id, "task-child-wait");
                assert_eq!(notification.child_status, "awaiting_approval");
                assert_eq!(
                    notification.failure_class,
                    Some(autonoetic_types::tool_error::FailureClass::ApprovalPending)
                );
            }
            other => panic!("expected child-state notification, got {other:?}"),
        }
    }

    #[test]
    fn child_state_notification_targets_nested_parent_session() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "nested-root", None).unwrap();

        let task = TaskRun {
            task_id: "task-nested-child".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "nested-root/intermediate/coder-x".to_string(),
            parent_session_id: "nested-root/intermediate".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-nested-child",
            TaskRunStatus::AwaitingApproval,
            Some("awaiting approval apr-nested".to_string()),
            Some(ApprovalMetadata {
                request_id: "apr-nested".to_string(),
                kind: "sandbox".to_string(),
                reason: Some("operator approval required".to_string()),
            }),
            None,
        )
        .unwrap();

        let root_notifications = store
            .list_notifications_for_session(
                &wf.root_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        assert!(
            root_notifications
                .iter()
                .all(|notification| notification.notification_type
                    != autonoetic_types::notification::NotificationType::ChildStateNotification),
            "child-state notification must not be queued only on the workflow root session"
        );

        let parent_notifications = store
            .list_notifications_for_session(
                &task.parent_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let child_signal = parent_notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::ChildStateNotification
            })
            .expect("nested parent session should receive the child-state notification");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(child_signal.payload.clone())
                .expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::ChildStateNotification { notification, .. } => {
                assert_eq!(notification.task_id, "task-nested-child");
                assert_eq!(notification.child_status, "awaiting_approval");
            }
            other => panic!("expected child-state notification, got {other:?}"),
        }
    }

    #[test]
    fn child_state_notification_falls_back_to_root_when_parent_session_missing() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "missing-parent-root", None)
            .unwrap();

        let task = TaskRun {
            task_id: "task-missing-parent".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "missing-parent-root/coder-x".to_string(),
            parent_session_id: String::new(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-missing-parent",
            TaskRunStatus::AwaitingApproval,
            Some("awaiting approval apr-root-fallback".to_string()),
            Some(ApprovalMetadata {
                request_id: "apr-root-fallback".to_string(),
                kind: "sandbox".to_string(),
                reason: Some("operator approval required".to_string()),
            }),
            None,
        )
        .unwrap();

        let root_notifications = store
            .list_notifications_for_session(
                &wf.root_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let child_signal = root_notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::ChildStateNotification
            })
            .expect("root session should receive the child-state notification fallback");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(child_signal.payload.clone())
                .expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::ChildStateNotification { notification, .. } => {
                assert_eq!(notification.task_id, "task-missing-parent");
                assert_eq!(notification.child_status, "awaiting_approval");
            }
            other => panic!("expected child-state notification, got {other:?}"),
        }
    }

    #[test]
    fn terminal_task_emits_child_resolved_event_and_notification() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "child-resolve-root", None)
            .unwrap();

        let task = TaskRun {
            task_id: "task-child-resolve".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "child-resolve-root/coder-x".to_string(),
            parent_session_id: "child-resolve-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-child-resolve",
            TaskRunStatus::Failed,
            Some("approval_rejected".to_string()),
            None,
            None,
        )
        .unwrap();

        let events = load_workflow_events(&cfg, Some(&store), &wf.workflow_id).unwrap();
        let resolved = events
            .iter()
            .find(|event| event.event_type == "workflow.child.resolved")
            .expect("workflow.child.resolved event should exist");
        assert_eq!(resolved.payload["task_id"], "task-child-resolve");
        assert_eq!(resolved.payload["child_status"], "failed");
        assert_eq!(resolved.payload["failure_class"], "policy_denied");

        // When the join is satisfied (single task completes → join triggers),
        // the child-state notification is coalesced into the
        // WorkflowJoinSatisfied signal — no separate ChildStateNotification
        // is queued. The join signal carries child_summaries instead.
        let notifications = store
            .list_notifications_for_session(
                &wf.root_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let join_signal = notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::WorkflowJoinSatisfied
            })
            .expect("workflow join satisfied notification should be queued");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(join_signal.payload.clone()).expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::WorkflowJoinSatisfied {
                child_summaries, ..
            } => {
                // The coalesced child summary should carry the task details.
                assert_eq!(child_summaries.len(), 1);
                assert_eq!(child_summaries[0].task_id, "task-child-resolve");
                assert_eq!(child_summaries[0].child_status, "failed");
                assert_eq!(
                    child_summaries[0].failure_class,
                    Some(autonoetic_types::tool_error::FailureClass::PolicyDenied)
                );
            }
            other => panic!("expected WorkflowJoinSatisfied, got {other:?}"),
        }
    }

    #[test]
    fn terminal_task_in_nested_session_emits_child_state_notification_to_parent() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "nested-terminal-root", None)
                .unwrap();

        let parent_session = "nested-terminal-root/intermediate";
        let child_session = "nested-terminal-root/intermediate/coder-x";

        let task = TaskRun {
            task_id: "task-nested-terminal".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: child_session.to_string(),
            parent_session_id: parent_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-nested-terminal",
            TaskRunStatus::Succeeded,
            Some("completed".to_string()),
            None,
            None,
        )
        .unwrap();

        // Root session receives the coalesced WorkflowJoinSatisfied signal.
        let root_notifications = store
            .list_notifications_for_session(
                &wf.root_session_id,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let join_signal = root_notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::WorkflowJoinSatisfied
            })
            .expect("root session should receive WorkflowJoinSatisfied");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(join_signal.payload.clone()).expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::WorkflowJoinSatisfied {
                child_summaries, ..
            } => {
                assert_eq!(child_summaries.len(), 1);
                assert_eq!(child_summaries[0].task_id, "task-nested-terminal");
                assert_eq!(child_summaries[0].child_status, "succeeded");
            }
            other => panic!("expected WorkflowJoinSatisfied, got {other:?}"),
        }

        // Intermediate parent session also receives a ChildStateNotification so it
        // can resume its own workflow_wait (regression test for the agent-factory
        // create_candidate -> promote stall).
        let parent_notifications = store
            .list_notifications_for_session(
                parent_session,
                autonoetic_types::notification::NotificationStatus::Pending,
            )
            .unwrap();
        let child_signal = parent_notifications
            .iter()
            .find(|notification| {
                notification.notification_type
                    == autonoetic_types::notification::NotificationType::ChildStateNotification
            })
            .expect("nested parent session should receive ChildStateNotification");
        let signal: crate::scheduler::signal::Signal =
            serde_json::from_value(child_signal.payload.clone()).expect("signal should deserialize");
        match signal {
            crate::scheduler::signal::Signal::ChildStateNotification { notification, .. } => {
                assert_eq!(notification.task_id, "task-nested-terminal");
                assert_eq!(notification.child_status, "succeeded");
            }
            other => panic!("expected ChildStateNotification, got {other:?}"),
        }
    }

    #[test]
    fn fail_running_tasks_for_session_transitions_matching_tasks() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "fail-tasks-root", None).unwrap();

        // Two tasks: one bound to the dead session, one bound to a different session.
        let dead_session = "fail-tasks-root/coder-dead".to_string();
        let live_session = "fail-tasks-root/coder-live".to_string();

        let dead_task = TaskRun {
            task_id: "task-dead".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: dead_session.clone(),
            parent_session_id: "fail-tasks-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        let live_task = TaskRun {
            task_id: "task-live".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: live_session.clone(),
            parent_session_id: "fail-tasks-root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &dead_task).unwrap();
        save_task_run(&cfg, Some(&store), &live_task).unwrap();

        // Fail tasks for the dead session.
        let failed = fail_running_tasks_for_session(
            &cfg,
            Some(&store),
            &dead_session,
            "spawn_execute_error",
        )
        .unwrap();
        assert_eq!(failed, 1, "exactly one task should have been transitioned");

        // Verify the dead task is now Failed.
        let updated_dead = load_task_run(&cfg, Some(&store), &wf.workflow_id, "task-dead")
            .unwrap()
            .unwrap();
        assert_eq!(updated_dead.status, TaskRunStatus::Failed);
        assert!(
            updated_dead
                .result_summary
                .as_deref()
                .unwrap_or("")
                .contains("child session terminated"),
            "result_summary should mention session termination: {:?}",
            updated_dead.result_summary
        );

        // The live task should still be Running.
        let updated_live = load_task_run(&cfg, Some(&store), &wf.workflow_id, "task-live")
            .unwrap()
            .unwrap();
        assert_eq!(updated_live.status, TaskRunStatus::Running);
    }

    // ---- RFC #775 Part A: typed failure taxonomy tests ----

    use autonoetic_types::task_completion::AgentOutcome;
    use autonoetic_types::tool_error::{FailureClass, RetryAdvice};

    fn dummy_task() -> TaskRun {
        TaskRun {
            task_id: "task-test".to_string(),
            workflow_id: "wf-test".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-test".to_string(),
            parent_session_id: "root".to_string(),
            status: TaskRunStatus::Running,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        }
    }

    #[test]
    fn build_notification_surfaces_clarification_needed_as_typed_outcome() {
        let task = dummy_task();
        let summary = r#"{"status":"clarification_needed","clarification_request":{"question":"which API?"}}"#;
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            Some(summary),
            None,
        );
        assert_eq!(notif.agent_outcome, Some(AgentOutcome::ClarificationNeeded));
        assert_eq!(notif.child_status, "succeeded");
        assert!(
            notif.failure_class.is_none(),
            "clarification_needed is penalty-free, not a failure_class"
        );
    }

    #[test]
    fn build_notification_detects_gave_up_on_empty_success() {
        let task = dummy_task();
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            None,
            None,
        );
        assert_eq!(notif.failure_class, Some(FailureClass::ChildGaveUp));
        assert_eq!(notif.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(notif.agent_outcome, None);
    }

    #[test]
    fn build_notification_detects_gave_up_on_blank_summary() {
        let task = dummy_task();
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            Some("   "),
            None,
        );
        assert_eq!(notif.failure_class, Some(FailureClass::ChildGaveUp));
    }

    #[test]
    fn build_notification_no_gave_up_when_summary_has_content() {
        let task = dummy_task();
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            Some("task completed successfully"),
            None,
        );
        assert!(
            notif.failure_class.is_none(),
            "non-empty summary should not trigger gave_up"
        );
        assert_eq!(notif.agent_outcome, None);
    }

    #[test]
    fn build_notification_no_gave_up_when_agent_outcome_parsed() {
        let task = dummy_task();
        // A pass verdict has a recognizable outcome — not gave_up even though
        // the summary is short.
        let summary = r#"{"status":"ok","evaluator_pass":true}"#;
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            Some(summary),
            None,
        );
        assert!(
            notif.failure_class.is_none(),
            "recognizable outcome should not trigger gave_up"
        );
    }

    #[test]
    fn output_contract_unmet_has_correct_metadata() {
        use crate::runtime::failure_classification::metadata_for_failure_class;
        let meta = metadata_for_failure_class(FailureClass::OutputContractUnmet);
        assert_eq!(meta.failure_class, Some(FailureClass::OutputContractUnmet));
        assert_eq!(meta.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(meta.retryable, Some(false));
    }

    #[test]
    fn child_gave_up_has_correct_metadata() {
        use crate::runtime::failure_classification::metadata_for_failure_class;
        let meta = metadata_for_failure_class(FailureClass::ChildGaveUp);
        assert_eq!(meta.failure_class, Some(FailureClass::ChildGaveUp));
        assert_eq!(meta.retry_advice, Some(RetryAdvice::DoNotRetry));
        assert_eq!(meta.retryable, Some(false));
    }

    // ---- RFC #776 Part B.1: output contract existence check tests ----

    #[test]
    fn check_output_contract_all_covered() {
        let expected = vec!["main.py".to_string(), "SKILL.md".to_string()];
        let produced = vec!["main.py".to_string(), "SKILL.md".to_string()];
        let unmet = check_output_contract(&expected, &produced, &[]);
        assert!(unmet.is_empty());
    }

    #[test]
    fn check_output_contract_finds_missing() {
        let expected = vec!["main.py".to_string(), "SKILL.md".to_string(), "tests.py".to_string()];
        let produced = vec!["main.py".to_string()];
        let unmet = check_output_contract(&expected, &produced, &[]);
        assert_eq!(unmet, vec!["SKILL.md".to_string(), "tests.py".to_string()]);
    }

    #[test]
    fn check_output_contract_covers_artifact_files() {
        let expected = vec!["handler.py".to_string()];
        let content = vec![];
        let artifacts = vec!["handler.py".to_string(), "utils.py".to_string()];
        let unmet = check_output_contract(&expected, &content, &artifacts);
        assert!(unmet.is_empty(), "expected output found in artifact files");
    }

    #[test]
    fn check_output_contract_empty_expected_is_clean() {
        let unmet = check_output_contract(&[], &["main.py".to_string()], &[]);
        assert!(unmet.is_empty());
    }

    #[test]
    fn notification_stamps_output_contract_unmet_from_metadata() {
        let mut task = dummy_task();
        task.metadata = Some(serde_json::json!({
            "output_contract_check": {
                "unmet": ["missing_file.py"],
            }
        }));
        let notif = build_child_state_notification(
            &task,
            TaskRunStatus::Succeeded,
            Some("{\"status\":\"ok\"}"),
            None,
        );
        assert_eq!(notif.failure_class, Some(FailureClass::OutputContractUnmet));
    }

    // ------------------------------------------------------------------
    // Child-wait suspend/resume robustness (sibling-deadlock fix)
    // ------------------------------------------------------------------

    /// A queued-not-started (`Pending`) task can be finalized directly —
    /// operator force-complete, abnormal close, or singleton-slot release on
    /// a bypassed scheduler. Refusing Pending → Succeeded/Failed silently
    /// strands singleton slots (the transition guard is an Ok-noop).
    #[test]
    fn update_task_run_status_allows_pending_to_terminal() {
        let (_dir, cfg, store) = child_wait_setup();
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), "pending-terminal-root", None).unwrap();
        let session = "pending-terminal-root/x".to_string();
        save_task_run(
            &cfg,
            Some(&store),
            &mk_task(&wf.workflow_id, "task-pending", &session, "pending-terminal-root", TaskRunStatus::Pending),
        )
        .unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf.workflow_id,
            "task-pending",
            TaskRunStatus::Succeeded,
            Some("finalized without running".to_string()),
            None,
            None,
        )
        .unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf.workflow_id, "task-pending")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, TaskRunStatus::Succeeded);
    }

    fn child_wait_setup() -> (tempfile::TempDir, GatewayConfig, crate::scheduler::gateway_store::GatewayStore) {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let gateway_dir = agents.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        (dir, cfg, store)
    }

    fn mk_task(
        workflow_id: &str,
        task_id: &str,
        session_id: &str,
        parent_session_id: &str,
        status: TaskRunStatus,
    ) -> TaskRun {
        TaskRun {
            task_id: task_id.to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "agent.default".to_string(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            status,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        }
    }

    /// The park/wake predicate is parent-scoped and status-authoritative:
    /// a session's wait set is exactly the non-terminal tasks IT spawned —
    /// never siblings, never terminal children.
    #[test]
    fn session_has_non_terminal_children_is_parent_scoped() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-pred";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let leaf = format!("{root}/static_evaluator");
        let sibling = format!("{root}/auditor");
        // The leaf's own task (parent = root) and a running sibling (parent = root).
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-leaf", &leaf, root, TaskRunStatus::Running)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-sibling", &sibling, root, TaskRunStatus::Running)).unwrap();

        // The leaf spawned nothing: no wait set, even though a sibling is active.
        assert!(
            !session_has_non_terminal_children(&cfg, Some(&store), &wf_id, &leaf).unwrap(),
            "leaf session must NOT park on an active sibling (session-d484ea13 deadlock)"
        );
        // The root spawned both: it DOES have a wait set.
        assert!(
            session_has_non_terminal_children(&cfg, Some(&store), &wf_id, root).unwrap(),
            "root session must park while its spawned children are non-terminal"
        );

        // Once the children are terminal-for-join, the root's wait set empties.
        update_task_run_status(&cfg, Some(&store), &wf_id, "task-leaf", TaskRunStatus::Succeeded, None, None, None).unwrap();
        update_task_run_status(&cfg, Some(&store), &wf_id, "task-sibling", TaskRunStatus::Failed, None, None, None).unwrap();
        assert!(
            !session_has_non_terminal_children(&cfg, Some(&store), &wf_id, root).unwrap(),
            "terminal children must not count toward the wait set"
        );
    }

    /// Production deadlock repro (session-d484ea13): a task parked in
    /// `paused_child_wait` because a SIBLING was active must be woken when the
    /// last sibling reaches a terminal state — the wake is condition-based,
    /// not direct-parent-scoped.
    #[test]
    fn sibling_terminal_transition_wakes_parked_task_with_empty_wait_set() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-sibling-wake";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parked_session = format!("{root}/static_evaluator");
        let sibling_session = format!("{root}/auditor");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parked", &parked_session, root, TaskRunStatus::Paused)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-sibling", &sibling_session, root, TaskRunStatus::Running)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parked",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused", "reason": "awaiting_async_child_completion"}),
        )
        .unwrap();

        update_task_run_status(
            &cfg,
            Some(&store),
            &wf_id,
            "task-sibling",
            TaskRunStatus::Succeeded,
            Some("audit pass".to_string()),
            None,
            None,
        )
        .unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf_id, "task-parked").unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskRunStatus::Runnable,
            "sibling-parked task with an empty wait set must wake on the sibling's terminal transition"
        );
    }

    /// A parked task that still has non-terminal children of its OWN must stay
    /// parked when a sibling resolves, and wake only when its own wait set
    /// empties.
    #[test]
    fn parked_task_with_running_children_not_woken_by_sibling() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-real-children";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parent_session = format!("{root}/agent-factory");
        let own_child_session = format!("{parent_session}/builder");
        let sibling_session = format!("{root}/auditor");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parent", &parent_session, root, TaskRunStatus::Paused)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-own-child", &own_child_session, &parent_session, TaskRunStatus::Running)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-sibling", &sibling_session, root, TaskRunStatus::Running)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parent",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused"}),
        )
        .unwrap();

        // Sibling resolves: parent still has a running child → stays Paused.
        update_task_run_status(&cfg, Some(&store), &wf_id, "task-sibling", TaskRunStatus::Succeeded, None, None, None).unwrap();
        let after_sibling = load_task_run(&cfg, Some(&store), &wf_id, "task-parent").unwrap().unwrap();
        assert_eq!(
            after_sibling.status,
            TaskRunStatus::Paused,
            "parked task with running children must NOT wake on a sibling's transition"
        );

        // Own child resolves: wait set empty → wakes.
        update_task_run_status(&cfg, Some(&store), &wf_id, "task-own-child", TaskRunStatus::Succeeded, None, None, None).unwrap();
        let after_child = load_task_run(&cfg, Some(&store), &wf_id, "task-parent").unwrap().unwrap();
        assert_eq!(
            after_child.status,
            TaskRunStatus::Runnable,
            "parked task must wake once its own wait set empties"
        );
    }

    /// An approval-timed-out (Stale) child must also wake eligible parked
    /// tasks — `Stale` is terminal-for-join even though not fully terminal.
    #[test]
    fn stale_child_wakes_parked_task_with_empty_wait_set() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-stale-wake";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parked_session = format!("{root}/evaluator");
        let child_session = format!("{root}/coder");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parked", &parked_session, root, TaskRunStatus::Paused)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-child", &child_session, root, TaskRunStatus::AwaitingApproval)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parked",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused"}),
        )
        .unwrap();

        update_task_run_status(&cfg, Some(&store), &wf_id, "task-child", TaskRunStatus::Stale, None, None, None).unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf_id, "task-parked").unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskRunStatus::Runnable,
            "Stale (approval timeout) is terminal-for-join and must wake parked tasks"
        );
    }

    /// One terminal event wakes ALL eligible parked tasks, not just the first.
    #[test]
    fn single_terminal_event_wakes_all_eligible_parked_tasks() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-multi-wake";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        for (task_id, session) in [
            ("task-parked-1", format!("{root}/evaluator")),
            ("task-parked-2", format!("{root}/auditor")),
        ] {
            save_task_run(&cfg, Some(&store), &mk_task(&wf_id, task_id, &session, root, TaskRunStatus::Paused)).unwrap();
            checkpoint_task(
                &cfg,
                Some(&store),
                &wf_id,
                task_id,
                "paused_child_wait".to_string(),
                serde_json::json!({"status": "paused"}),
            )
            .unwrap();
        }
        let sibling_session = format!("{root}/coder");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-sibling", &sibling_session, root, TaskRunStatus::Running)).unwrap();

        update_task_run_status(&cfg, Some(&store), &wf_id, "task-sibling", TaskRunStatus::Succeeded, None, None, None).unwrap();

        for task_id in ["task-parked-1", "task-parked-2"] {
            let after = load_task_run(&cfg, Some(&store), &wf_id, task_id).unwrap().unwrap();
            assert_eq!(
                after.status,
                TaskRunStatus::Runnable,
                "every eligible parked task must wake on the same terminal event ({task_id})"
            );
        }
    }

    /// Terminal saves must not (re-)add the task to `active_task_ids`;
    /// non-terminal saves must.
    #[test]
    fn save_task_run_keeps_terminal_tasks_out_of_active_task_ids() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-active-hygiene";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();
        let session = format!("{root}/coder");

        let task = mk_task(&wf_id, "task-x", &session, root, TaskRunStatus::Running);
        save_task_run(&cfg, Some(&store), &task).unwrap();
        let run = load_workflow_run(&cfg, Some(&store), &wf_id).unwrap().unwrap();
        assert!(run.active_task_ids.contains(&"task-x".to_string()));

        let mut terminal = task.clone();
        terminal.status = TaskRunStatus::Failed;
        save_task_run(&cfg, Some(&store), &terminal).unwrap();
        let run = load_workflow_run(&cfg, Some(&store), &wf_id).unwrap().unwrap();
        assert!(
            !run.active_task_ids.contains(&"task-x".to_string()),
            "terminal save must remove the task from active_task_ids"
        );
    }

    /// Janitor: a parked child-wait task whose wait set is empty is woken even
    /// when no terminal transition fired (missed-wake safety net).
    #[test]
    fn janitor_wakes_orphaned_child_wait_task() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-janitor-wake";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parked_session = format!("{root}/evaluator");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parked", &parked_session, root, TaskRunStatus::Paused)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parked",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused"}),
        )
        .unwrap();

        reconcile_paused_child_wait_tasks(&cfg, Some(&store)).unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf_id, "task-parked").unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskRunStatus::Runnable,
            "janitor must wake a parked child-wait task whose wait set is empty"
        );
    }

    /// Janitor: a parked task with running children stays parked.
    #[test]
    fn janitor_keeps_parked_task_with_running_children() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-janitor-keep";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parent_session = format!("{root}/agent-factory");
        let child_session = format!("{parent_session}/builder");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parent", &parent_session, root, TaskRunStatus::Paused)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-child", &child_session, &parent_session, TaskRunStatus::Running)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parent",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused"}),
        )
        .unwrap();

        reconcile_paused_child_wait_tasks(&cfg, Some(&store)).unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf_id, "task-parent").unwrap().unwrap();
        assert_eq!(after.status, TaskRunStatus::Paused);
    }

    /// Janitor: a child-wait task parked in a terminal workflow can never be
    /// answered — fail it instead of stranding it forever.
    #[test]
    fn janitor_fails_child_wait_task_in_terminal_workflow() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-janitor-terminal";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let parked_session = format!("{root}/evaluator");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-parked", &parked_session, root, TaskRunStatus::Paused)).unwrap();
        checkpoint_task(
            &cfg,
            Some(&store),
            &wf_id,
            "task-parked",
            "paused_child_wait".to_string(),
            serde_json::json!({"status": "paused"}),
        )
        .unwrap();

        let mut run = load_workflow_run(&cfg, Some(&store), &wf_id).unwrap().unwrap();
        run.status = autonoetic_types::workflow::WorkflowRunStatus::Completed;
        save_workflow_run(&cfg, Some(&store), &run).unwrap();

        reconcile_paused_child_wait_tasks(&cfg, Some(&store)).unwrap();

        let after = load_task_run(&cfg, Some(&store), &wf_id, "task-parked").unwrap().unwrap();
        assert_eq!(
            after.status,
            TaskRunStatus::Failed,
            "child-wait task parked in a terminal workflow must be failed, not stranded"
        );
    }

    /// Janitor: drifted `active_task_ids` (terminal entries never dequeued,
    /// live entries lost) is repaired in place.
    #[test]
    fn janitor_repairs_active_task_ids_drift() {
        let (_dir, cfg, store) = child_wait_setup();
        let root = "root-janitor-drift";
        let wf = ensure_workflow_for_root_session(&cfg, Some(&store), root, None).unwrap();
        let wf_id = wf.workflow_id.clone();

        let live_session = format!("{root}/coder");
        let dead_session = format!("{root}/auditor");
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-live", &live_session, root, TaskRunStatus::Running)).unwrap();
        save_task_run(&cfg, Some(&store), &mk_task(&wf_id, "task-dead", &dead_session, root, TaskRunStatus::Succeeded)).unwrap();

        // Corrupt: dead entry present, live entry missing.
        let mut run = load_workflow_run(&cfg, Some(&store), &wf_id).unwrap().unwrap();
        run.active_task_ids = vec!["task-dead".to_string()];
        save_workflow_run(&cfg, Some(&store), &run).unwrap();

        reconcile_paused_child_wait_tasks(&cfg, Some(&store)).unwrap();

        let run = load_workflow_run(&cfg, Some(&store), &wf_id).unwrap().unwrap();
        assert_eq!(
            run.active_task_ids,
            vec!["task-live".to_string()],
            "janitor must drop terminal entries and re-add live ones"
        );
    }
}
