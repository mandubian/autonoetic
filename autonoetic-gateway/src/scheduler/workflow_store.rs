//! Durable workflow / task persistence under the gateway scheduler directory.
//!
//! Layout (under `<agents_dir>/.gateway/scheduler/workflows/`):
//! - `index/by_root/<sha256-hex>.json` — maps stable root key → `workflow_id`
//! - `runs/<workflow_id>/workflow.json` — [`WorkflowRun`](autonoetic_types::workflow::WorkflowRun)
//! - `runs/<workflow_id>/tasks/<task_id>.json` — [`TaskRun`](autonoetic_types::workflow::TaskRun)
//! - Workflow events are stored in SQLite table `workflow_events` (`gateway.db`).

use crate::execution::gateway_root_dir;
use crate::runtime::live_digest::base_session_id;
use crate::scheduler::gateway_store::GatewayStore;
use crate::scheduler::store::{read_json_file, write_json_file};
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{
    QueuedTaskRun, TaskCheckpoint, TaskRun, TaskRunStatus, WorkflowCheckpoint, WorkflowEventRecord,
    WorkflowRun, WorkflowRunStatus,
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
    format!("wf-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

/// Allocate a new `task-*` id (separate from session paths).
pub fn new_task_id() -> String {
    format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8])
}

fn new_event_id() -> String {
    format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8])
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
    let owned_store;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;
            &owned_store
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
    let owned_store;
    let store = match store {
        Some(s) => s,
        None => {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            owned_store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?;
            &owned_store
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
    if let Err(e) = refresh_workflow_graph_markdown(config, Some(store), &event.workflow_id) {
        tracing::warn!(
            target: "session_timeline",
            workflow_id = %event.workflow_id,
            error = %e,
            "Failed to refresh workflow_graph.md"
        );
    }
    Ok(())
}

/// Write or replace a task record and refresh `workflow.active_task_ids`.
pub fn save_task_run(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    task: &TaskRun,
) -> anyhow::Result<()> {
    let path = task_run_path(config, &task.workflow_id, &task.task_id);
    write_json_file(&path, task)?;

    let mut run = load_workflow_run(config, _store, &task.workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", task.workflow_id))?;
    if !run.active_task_ids.contains(&task.task_id) {
        run.active_task_ids.push(task.task_id.clone());
    }
    run.updated_at = now_rfc3339();
    save_workflow_run(config, _store, &run)?;
    Ok(())
}

/// Load a task run if the file exists.
pub fn load_task_run(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskRun>> {
    let p = task_run_path(config, workflow_id, task_id);
    if !p.exists() {
        return Ok(None);
    }
    Ok(Some(read_json_file(&p)?))
}

/// List all persisted [`TaskRun`] records under `runs/<workflow_id>/tasks/`.
pub fn list_task_runs_for_workflow(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<TaskRun>> {
    let dir: PathBuf = workflow_run_dir(config, workflow_id).join("tasks");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<TaskRun> = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_json_file::<TaskRun>(&path) {
            Ok(t) => out.push(t),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip invalid task json");
            }
        }
    }
    out.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.task_id.cmp(&b.task_id))
    });
    Ok(out)
}

/// Update task status (and optional result summary) and append a completion-style event.
pub fn update_task_run_status(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    status: TaskRunStatus,
    result_summary: Option<String>,
) -> anyhow::Result<()> {
    let mut task = load_task_run(config, store, workflow_id, task_id)?
        .ok_or_else(|| anyhow::anyhow!("task '{}' not in workflow '{}'", task_id, workflow_id))?;

    // Store previous status for implicit artifact creation
    let was_succeeded = task.status == TaskRunStatus::Succeeded;
    let is_now_succeeded = status == TaskRunStatus::Succeeded;

    task.status = status;
    task.updated_at = now_rfc3339();
    task.result_summary = result_summary.clone();
    save_task_run(config, store, &task)?;

    // Create implicit artifact when task succeeds (transition to Succeeded)
    if is_now_succeeded && !was_succeeded {
        if let Err(e) = create_implicit_artifact(config, &task, result_summary.as_deref()) {
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

    let payload = serde_json::json!({ "status": status });

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

    if let Some(wf) = load_workflow_run(config, store, workflow_id)? {
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
            &wf.root_session_id,
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

        // Check join condition after terminal task updates
        let is_terminal = matches!(
            status,
            TaskRunStatus::Succeeded
                | TaskRunStatus::Failed
                | TaskRunStatus::Cancelled
                | TaskRunStatus::Aborted
        );
        let wf_not_emergency_stopped = !matches!(
            wf.status,
            WorkflowRunStatus::EmergencyStopping | WorkflowRunStatus::EmergencyStopped
        );
        if is_terminal && wf_not_emergency_stopped && !wf.join_task_ids.is_empty() {
            if let Ok(true) = check_join_condition(config, store, workflow_id) {
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

                // Send a signal to the planner session to resume
                if let Err(e) = crate::scheduler::signal::send_workflow_join_satisfied(
                    store,
                    &wf_mut.root_session_id,
                    workflow_id,
                    wf_mut.join_task_ids.clone(),
                ) {
                    tracing::warn!(
                        target: "signal",
                        error = %e,
                        "Failed to send workflow join satisfied signal"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Creates an implicit artifact reference for a completed task.
///
/// This stores a minimal JSON structure with task output information
/// that can be retrieved via workflow.wait by the parent session.
fn create_implicit_artifact(
    config: &GatewayConfig,
    task: &TaskRun,
    result_summary: Option<&str>,
) -> anyhow::Result<()> {
    use crate::runtime::content_store::{ContentStore, ContentVisibility};

    let gw_dir = crate::execution::gateway_root_dir(config);
    let content_store = ContentStore::new(&gw_dir)?;

    // Generate implicit artifact ID
    let artifact_id = format!("impl_{}", task.task_id);

    // Build implicit artifact metadata
    let implicit_data = serde_json::json!({
        "artifact_id": artifact_id,
        "artifact_type": "implicit",
        "task_id": task.task_id,
        "agent_id": task.agent_id,
        "session_id": task.session_id,
        "parent_session": task.parent_session_id,
        "created_at": task.updated_at,
        "summary": result_summary.unwrap_or("Task completed"),
    });

    // Write as session-visible content in parent session
    let json_bytes = serde_json::to_vec_pretty(&implicit_data)?;
    let handle = content_store.write(&json_bytes)?;

    // Register with session visibility for parent session access
    let parent_session = &task.parent_session_id;
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
        "Created implicit artifact for completed task"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Async task queue
// ---------------------------------------------------------------------------

fn queue_dir(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id).join("queue")
}

fn queued_task_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    queue_dir(config, workflow_id).join(format!("{task_id}.json"))
}

fn task_claim_path(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> PathBuf {
    queue_dir(config, workflow_id).join(format!("{task_id}.claim.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskExecutionClaim {
    pub workflow_id: String,
    pub task_id: String,
    pub scheduler_instance_id: String,
    pub claimed_at: String,
    pub heartbeat_at: String,
}

fn parse_rfc3339(ts: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(ts)?.with_timezone(&Utc))
}

fn claim_is_stale(claim: &TaskExecutionClaim, stale_after_secs: u64) -> bool {
    let Ok(heartbeat_at) = parse_rfc3339(&claim.heartbeat_at) else {
        return true;
    };
    Utc::now() - heartbeat_at > Duration::seconds(stale_after_secs as i64)
}

pub fn load_task_claim(
    config: &GatewayConfig,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<Option<TaskExecutionClaim>> {
    let path = task_claim_path(config, workflow_id, task_id);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(read_json_file(&path)?))
}

pub fn acquire_task_claim(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
    stale_after_secs: u64,
) -> anyhow::Result<Option<TaskExecutionClaim>> {
    if let Some(existing) = load_task_claim(config, workflow_id, task_id)? {
        if !claim_is_stale(&existing, stale_after_secs) {
            return Ok(None);
        }
    }

    let claim = TaskExecutionClaim {
        workflow_id: workflow_id.to_string(),
        task_id: task_id.to_string(),
        scheduler_instance_id: uuid::Uuid::new_v4().to_string(),
        claimed_at: now_rfc3339(),
        heartbeat_at: now_rfc3339(),
    };
    write_json_file(&task_claim_path(config, workflow_id, task_id), &claim)?;
    Ok(Some(claim))
}

pub fn refresh_task_claim_heartbeat(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let Some(mut claim) = load_task_claim(config, workflow_id, task_id)? else {
        return Ok(());
    };
    claim.heartbeat_at = now_rfc3339();
    write_json_file(&task_claim_path(config, workflow_id, task_id), &claim)
}

pub fn release_task_claim(
    config: &GatewayConfig,
    _store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let path = task_claim_path(config, workflow_id, task_id);
    if path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn queued_task_exists(config: &GatewayConfig, workflow_id: &str, task_id: &str) -> bool {
    queued_task_path(config, workflow_id, task_id).exists()
}

/// When a queue file already exists but the task checkpoint has a newer `resume_message`
/// (e.g. after `approval_resolved`), update the queued task payload in place.
pub fn refresh_queued_task_message_from_task_checkpoint(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    if !queued_task_exists(config, workflow_id, task_id) {
        return Ok(());
    }
    let Some(cp) = load_task_checkpoint(config, store, workflow_id, task_id)? else {
        return Ok(());
    };
    if cp.step != "approval_resolved" {
        return Ok(());
    }
    let resume_raw = cp.state.get("resume_message");
    let rm = match resume_raw {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v)?,
        None => return Ok(()),
    };
    let path = queued_task_path(config, workflow_id, task_id);
    let mut q: QueuedTaskRun = read_json_file(&path)?;
    if q.message != rm {
        q.message = rm;
        write_json_file(&path, &q)?;
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
    let dir = queue_dir(config, &queued.workflow_id);
    fs::create_dir_all(&dir)?;
    let path = queued_task_path(config, &queued.workflow_id, &queued.task_id);
    write_json_file(&path, queued)?;

    let mut run = load_workflow_run(config, store, &queued.workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", queued.workflow_id))?;
    anyhow::ensure!(
        run.status != WorkflowRunStatus::EmergencyStopping
            && run.status != WorkflowRunStatus::EmergencyStopped,
        "workflow '{}' is in emergency stop; refusing new queued work",
        queued.workflow_id
    );
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
    save_workflow_run(config, store, &run)?;

    append_workflow_event(
        config,
        store,
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
    )?;

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
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let path = queued_task_path(config, workflow_id, task_id);
    if path.exists() {
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!(
                target: "workflow",
                path = %path.display(),
                error = %e,
                "Failed to remove queued task file"
            );
        }
    }

    let mut run = match load_workflow_run(config, _store, workflow_id)? {
        Some(r) => r,
        None => return Ok(()),
    };
    run.queued_task_ids.retain(|id| id != task_id);
    run.updated_at = now_rfc3339();
    save_workflow_run(config, _store, &run)?;
    release_task_claim(config, _store, workflow_id, task_id)?;
    Ok(())
}

/// Load all queued tasks for a workflow.
pub fn load_queued_tasks(
    config: &GatewayConfig,
    _store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
) -> anyhow::Result<Vec<QueuedTaskRun>> {
    let dir = queue_dir(config, workflow_id);
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
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.ends_with(".claim.json"))
        {
            continue;
        }
        match read_json_file::<QueuedTaskRun>(&path) {
            Ok(q) => out.push(q),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "skip invalid queued task json");
            }
        }
    }
    out.sort_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at));
    Ok(out)
}

/// Load ALL queued tasks across all workflows (for the scheduler tick).
/// Scans the runs/ directory for any workflow with a non-empty queue/.
pub fn load_all_queued_tasks(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
) -> anyhow::Result<Vec<QueuedTaskRun>> {
    let root = workflows_root(config).join("runs");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let wf_id = entry.file_name().to_string_lossy().to_string();
        let queued = load_queued_tasks(config, store, &wf_id)?;
        out.extend(queued);
    }
    out.sort_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at));
    Ok(out)
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

    // Check each group: if ALL tasks in ANY group are terminal, join is satisfied
    for (_group, task_ids) in &groups {
        let mut all_terminal = true;
        for task_id in task_ids {
            match load_task_run(config, store, workflow_id, task_id)? {
                Some(task) => match task.status {
                    TaskRunStatus::Succeeded
                    | TaskRunStatus::Failed
                    | TaskRunStatus::Cancelled
                    | TaskRunStatus::Aborted => {
                        continue;
                    }
                    _ => {
                        all_terminal = false;
                        break;
                    }
                },
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
        let terminal = matches!(
            task.status,
            TaskRunStatus::Succeeded
                | TaskRunStatus::Failed
                | TaskRunStatus::Cancelled
                | TaskRunStatus::Aborted
        );
        if terminal {
            continue;
        }
        let _ = release_task_claim(config, store, workflow_id, &task.task_id);
        if let Err(e) = crate::runtime::continuation::delete_continuation(config, &task.task_id) {
            tracing::debug!(
                target: "workflow",
                task_id = %task.task_id,
                error = %e,
                "continuation delete during emergency stop (may be absent)"
            );
        }

        task.status = TaskRunStatus::Aborted;
        task.updated_at = now_rfc3339();
        task.result_summary = Some(format!("emergency_stop:{stop_id}"));
        save_task_run(config, store, &task)?;

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

/// Generate a compact, single-line summary of a workflow's current state.
/// Returns `None` if no workflow exists for the given root session.
/// This is intended for injecting into chat at turn boundaries.
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
            TaskRunStatus::Failed | TaskRunStatus::Cancelled | TaskRunStatus::Aborted => failed += 1,
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
    !matches!(
        run.status,
        WorkflowRunStatus::Completed
            | WorkflowRunStatus::Failed
            | WorkflowRunStatus::Cancelled
            | WorkflowRunStatus::EmergencyStopping
            | WorkflowRunStatus::EmergencyStopped
    )
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
        let Some(run) = load_workflow_run(config, store, &workflow_id)? else {
            continue;
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
        };
        save_task_run(&cfg, None, &task).unwrap();
        update_task_run_status(
            &cfg,
            None,
            &wf.workflow_id,
            &tid,
            TaskRunStatus::Succeeded,
            Some("ok".to_string()),
        )
        .unwrap();
        let loaded = load_task_run(&cfg, None, &wf.workflow_id, &tid)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Succeeded);
        assert_eq!(loaded.result_summary.as_deref(), Some("ok"));
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
        )
        .unwrap();

        let loaded = load_task_run(&cfg, None, &wf.workflow_id, "task-rej")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, TaskRunStatus::Failed);

        let events = load_workflow_events(&cfg, None, &wf.workflow_id).unwrap();
        assert!(events.iter().any(|e| e.event_type == "task.failed"));
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

        let loaded = load_task_claim(&cfg, &wf.workflow_id, "task-c1")
            .unwrap()
            .expect("claim present");
        assert_eq!(loaded.scheduler_instance_id, claim.scheduler_instance_id);

        release_task_claim(&cfg, None, &wf.workflow_id, "task-c1").unwrap();
        assert!(load_task_claim(&cfg, &wf.workflow_id, "task-c1")
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
}
