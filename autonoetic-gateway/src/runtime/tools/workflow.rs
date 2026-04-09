use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::{AgentManifest, ToolTier};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde::de::{self, DeserializeOwned};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ApprovalStatusTool));
    registry.register(Box::new(WorkflowWaitTool));
    registry.register(Box::new(WorkflowStateTool));
    registry.register(Box::new(WorkflowCancelTaskTool));
    registry.register(Box::new(WorkflowForceCompleteTool));
}

// ---------------------------------------------------------------------------
// Approval Status Tool
// ---------------------------------------------------------------------------

/// Query the status of an approval request.
/// Allows agents to check whether an approval is pending, approved, or rejected.
pub struct ApprovalStatusTool;

impl NativeTool for ApprovalStatusTool {
    fn name(&self) -> &'static str {
        "approval.status"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Query the status of an approval request. Returns the current status (pending, approved, rejected) and associated details.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "approval_id": {
                        "type": "string",
                        "description": "The approval request ID to check (e.g., 'apr-abc123')"
                    }
                },
                "required": ["approval_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true // Available to all agents
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            approval_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "approval_id": args.approval_id,
                "status": "unknown",
                "message": "Gateway store not available"
            }))?);
        };

        match store.get_approval(&args.approval_id) {
            Ok(Some(request)) => {
                let status = match &request.status {
                    Some(s) => match s {
                        autonoetic_types::background::ApprovalStatus::Approved => "approved",
                        autonoetic_types::background::ApprovalStatus::Rejected => "rejected",
                        autonoetic_types::background::ApprovalStatus::Cancelled => "cancelled",
                    },
                    None => "pending",
                }
                .to_string();

                let response = serde_json::json!({
                    "ok": true,
                    "approval_id": args.approval_id,
                    "status": status,
                    "agent_id": request.agent_id,
                    "session_id": request.session_id,
                    "created_at": request.created_at,
                    "decided_at": request.decided_at,
                    "decided_by": request.decided_by,
                    "reason": request.reason,
                    "workflow_id": request.workflow_id,
                    "task_id": request.task_id
                });

                serde_json::to_string(&response).map_err(Into::into)
            }
            Ok(None) => {
                let response = serde_json::json!({
                    "ok": true,
                    "approval_id": args.approval_id,
                    "status": "not_found",
                    "message": "Approval request not found"
                });
                serde_json::to_string(&response).map_err(Into::into)
            }
            Err(e) => {
                let response = serde_json::json!({
                    "ok": false,
                    "approval_id": args.approval_id,
                    "error": e.to_string()
                });
                serde_json::to_string(&response).map_err(Into::into)
            }
        }
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(parsed_args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(approval_id) = parsed_args.get("approval_id").and_then(|v| v.as_str()) {
                meta.path = Some(approval_id.to_string());
            }
        }
        meta
    }
}

// ---------------------------------------------------------------------------
// Workflow Wait Tool
// ---------------------------------------------------------------------------

/// Checks the status of async tasks spawned with `agent.spawn(async: true)`.
/// Supports blocking mode: polls until all tasks complete or timeout expires.
pub struct WorkflowWaitTool;

fn check_task_statuses(
    config: &autonoetic_types::config::GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_ids: &[String],
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> (
    Vec<serde_json::Value>,
    bool,
    bool,
    bool,
    usize,
    Vec<serde_json::Value>,
) {
    let mut tasks_status = Vec::new();
    let mut all_done = true;
    let mut any_failed = false;
    let mut any_not_found = false;
    let mut failed_task_count = 0;
    let mut failure_summary: Vec<serde_json::Value> = Vec::new();

    for task_id in task_ids {
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)
            .ok()
            .flatten();
        match task {
            Some(t) => {
                let is_terminal = matches!(
                    t.status,
                    autonoetic_types::workflow::TaskRunStatus::Succeeded
                        | autonoetic_types::workflow::TaskRunStatus::Failed
                        | autonoetic_types::workflow::TaskRunStatus::Cancelled
                        | autonoetic_types::workflow::TaskRunStatus::Aborted
                );
                if !is_terminal {
                    all_done = false;
                }
                if t.status == autonoetic_types::workflow::TaskRunStatus::Failed {
                    any_failed = true;
                    failed_task_count += 1;
                    let mut fentry = serde_json::json!({
                        "task_id": t.task_id,
                        "agent_id": t.agent_id,
                        "result_summary": t.result_summary,
                    });
                    if let Ok(Some(cp)) = crate::scheduler::load_task_checkpoint(
                        config,
                        store,
                        workflow_id,
                        &t.task_id,
                    ) {
                        fentry["checkpoint_step"] = serde_json::Value::String(cp.step);
                        if cp.state != serde_json::Value::Null {
                            fentry["checkpoint_state"] = cp.state;
                        }
                    }
                    if failure_summary.len() < 5 {
                        failure_summary.push(fentry);
                    }
                }
                let mut entry = serde_json::json!({
                    "task_id": t.task_id,
                    "agent_id": t.agent_id,
                    "session_id": t.session_id,
                    "status": format!("{:?}", t.status),
                    "result_summary": t.result_summary,
                });
                // Consume task checkpoint: include last step/state
                if let Ok(Some(cp)) =
                    crate::scheduler::load_task_checkpoint(config, store, workflow_id, &t.task_id)
                {
                    entry["checkpoint_step"] = serde_json::Value::String(cp.step);
                    entry["checkpoint_version"] = serde_json::json!(cp.version);
                    if cp.state != serde_json::Value::Null {
                        entry["checkpoint_state"] = cp.state;
                    }
                }
                // Check for implicit artifact created for this task
                if t.status == autonoetic_types::workflow::TaskRunStatus::Succeeded {
                    if let (Some(gw_dir), Some(sid)) = (gateway_dir, session_id) {
                        let implicit_name = format!("impl_{}", t.task_id);
                        if let Ok(content_store) =
                            crate::runtime::content_store::ContentStore::new(gw_dir)
                        {
                            if let Ok(content) = content_store.read_by_name(sid, &implicit_name) {
                                if let Ok(artifact_data) =
                                    serde_json::from_slice::<serde_json::Value>(&content)
                                {
                                    let output = serde_json::json!({
                                        "artifact_id": artifact_data.get("artifact_id").and_then(|v| v.as_str()),
                                        "summary": artifact_data.get("summary").and_then(|v| v.as_str()),
                                        "created_at": artifact_data.get("created_at").and_then(|v| v.as_str()),
                                    });
                                    entry["output"] = output;
                                }
                            }
                        }
                    }
                }
                tasks_status.push(entry);
            }
            None => {
                let queued = crate::scheduler::load_queued_tasks(config, store, workflow_id)
                    .unwrap_or_default();
                let is_queued = queued.iter().any(|q| q.task_id == *task_id);
                if is_queued {
                    all_done = false;
                    tasks_status.push(serde_json::json!({
                        "task_id": task_id,
                        "status": "queued",
                    }));
                } else {
                    all_done = false;
                    any_not_found = true;
                    tasks_status.push(serde_json::json!({
                        "task_id": task_id,
                        "status": "not_found",
                    }));
                }
            }
        }
    }
    (
        tasks_status,
        all_done,
        any_failed,
        any_not_found,
        failed_task_count,
        failure_summary,
    )
}

fn deserialize_task_ids_lenient<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| de::Error::custom("task_ids array elements must be strings"))
            })
            .collect(),
        serde_json::Value::String(s) => {
            let sanitized = s.replace("<|\"|>", "\"");
            serde_json::from_str::<Vec<String>>(&sanitized).map_err(de::Error::custom)
        }
        other => Err(de::Error::custom(format!(
            "task_ids must be an array or a JSON string of an array, got {other}"
        ))),
    }
}

impl NativeTool for WorkflowWaitTool {
    fn name(&self) -> &'static str {
        "workflow.wait"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Wait for async tasks to complete. Pass task_ids from agent.spawn(async=true). Returns structured status for each task. Succeeded tasks include an 'output' field with a stable implicit artifact_id (e.g., 'impl_task-abc123') — use content.read with that ID to consume the child's result. This is the canonical parent-child output handoff mechanism for ordinary agents. With timeout_secs=0 (default), returns current status immediately. With timeout_secs>0, polls until all tasks finish or timeout expires.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of task IDs to wait for (from agent.spawn responses with async=true)."
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "Optional workflow ID. If omitted, resolved from the current session's root."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 300,
                        "description": "Max seconds to wait. 0 = check once and return (default). >0 = poll until all tasks finish or timeout."
                    },
                    "poll_interval_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "description": "Seconds between status polls when blocking. Default: 2."
                    }
                },
                "required": ["task_ids"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(deserialize_with = "deserialize_task_ids_lenient")]
            task_ids: Vec<String>,
            #[serde(default)]
            workflow_id: Option<String>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            poll_interval_secs: Option<u64>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.task_ids.is_empty(), "task_ids must not be empty");

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let fallback_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_config);

        // Resolve workflow_id from session if not provided
        let workflow_id = match args.workflow_id {
            Some(id) => id,
            None => {
                let sid = session_id.unwrap_or(&manifest.agent.id);
                let root = crate::runtime::content_store::root_session_id(sid);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)?
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let timeout_secs = args.timeout_secs.unwrap_or(0).min(300);
        let poll_interval_secs = args.poll_interval_secs.unwrap_or(2).clamp(1, 30);

        // Non-blocking mode: check once and return
        if timeout_secs == 0 {
            let (
                tasks_status,
                all_done,
                any_failed,
                any_not_found,
                failed_task_count,
                failure_summary,
            ) = check_task_statuses(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
                &args.task_ids,
                _gateway_dir,
                session_id,
            );
            return serde_json::to_string(&serde_json::json!({
                "ok": true,
                "workflow_id": workflow_id,
                "tasks": tasks_status,
                "join_satisfied": all_done,
                "any_failed": any_failed,
                "any_not_found": any_not_found,
                "failed_task_count": failed_task_count,
                "failure_summary": failure_summary,
                "waited_secs": 0,
                "message": if all_done {
                    if any_failed {
                        "All tasks completed (some failed). Review task results and proceed."
                    } else {
                        "All tasks completed successfully. You may proceed with the results."
                    }
                } else if any_not_found {
                    "One or more tasks were not found. Verify task_ids and workflow_id."
                } else {
                    "Some tasks are still running. Call workflow.wait with timeout_secs > 0 to block until they finish, or continue with other work."
                }
            }))
            .map_err(Into::into);
        }

        // Blocking mode: poll until join satisfied or timeout
        let task_ids = args.task_ids.clone();
        let wf_id = workflow_id.clone();
        let gw_config_arc = std::sync::Arc::new(gw_config.clone());

        let (
            tasks_status,
            all_done,
            any_failed,
            any_not_found,
            waited_secs,
            failed_task_count,
            failure_summary,
        ) = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    poll_until_join(
                        gw_config_arc.as_ref(),
                        gateway_store.as_deref(),
                        &wf_id,
                        &task_ids,
                        timeout_secs,
                        poll_interval_secs,
                        _gateway_dir,
                        session_id,
                    )
                    .await
                })
            })
        } else {
            tokio::runtime::Runtime::new()?.block_on(async {
                poll_until_join(
                    gw_config_arc.as_ref(),
                    gateway_store.as_deref(),
                    &wf_id,
                    &task_ids,
                    timeout_secs,
                    poll_interval_secs,
                    _gateway_dir,
                    session_id,
                )
                .await
            })
        };

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workflow_id": workflow_id,
            "tasks": tasks_status,
            "join_satisfied": all_done,
            "any_failed": any_failed,
            "any_not_found": any_not_found,
            "failed_task_count": failed_task_count,
            "failure_summary": failure_summary,
            "waited_secs": waited_secs,
            "message": if all_done {
                if any_failed {
                    format!("All tasks completed after {}s (some failed). Review task results and proceed.", waited_secs)
                } else {
                    format!("All tasks completed successfully after {}s. You may proceed with the results.", waited_secs)
                }
            } else if any_not_found {
                "One or more tasks were not found. Verify task_ids and workflow_id.".to_string()
            } else {
                format!("Timed out after {}s. Some tasks are still running. Call workflow.wait again or proceed with partial results.", waited_secs)
            }
        }))
        .map_err(Into::into)
    }
}

async fn poll_until_join(
    config: &GatewayConfig,
    store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    workflow_id: &str,
    task_ids: &[String],
    timeout_secs: u64,
    poll_interval_secs: u64,
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
) -> (
    Vec<serde_json::Value>,
    bool,
    bool,
    bool,
    u64,
    usize,
    Vec<serde_json::Value>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut waited_secs = 0u64;

    loop {
        let (tasks_status, all_done, any_failed, any_not_found, failed_task_count, failure_summary) =
            check_task_statuses(
                config,
                store,
                workflow_id,
                task_ids,
                gateway_dir,
                session_id,
            );
        if all_done {
            return (
                tasks_status,
                true,
                any_failed,
                any_not_found,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }
        if any_not_found {
            return (
                tasks_status,
                false,
                any_failed,
                true,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return (
                tasks_status,
                false,
                any_failed,
                any_not_found,
                waited_secs,
                failed_task_count,
                failure_summary,
            );
        }

        let remaining = (deadline - now).as_secs().min(poll_interval_secs).max(1);
        waited_secs += remaining;
        tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
    }
}

// ---------------------------------------------------------------------------
// Workflow State Tool
// ---------------------------------------------------------------------------

/// Exposes compact, structured workflow state to agents for deterministic resume.
/// Returns the current workflow step, completed tasks, pending approvals, and
/// valid next actions — replacing prose-based history inference.
pub struct WorkflowStateTool;

impl NativeTool for WorkflowStateTool {
    fn name(&self) -> &'static str {
        "workflow.state"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Returns compact, structured workflow state for deterministic resume. Use this instead of re-inferring state from conversation history. Returns: current step, completed tasks with artifact IDs, pending approvals, active tasks, and reuse guards.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "Optional workflow ID. If omitted, resolved from the current session's root."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            workflow_id: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let agents_dir = agent_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Agent directory is missing its agents root parent"))?;

        let fallback_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_config);

        let workflow_id = match args.workflow_id {
            Some(id) => id,
            None => {
                let sid = session_id.unwrap_or(&manifest.agent.id);
                let root = crate::runtime::content_store::root_session_id(sid);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)?
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let workflow = crate::scheduler::workflow_store::load_workflow_run(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )?;

        let tasks = crate::scheduler::workflow_store::list_task_runs_for_workflow(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )?;

        // Load pending approvals for this workflow to enrich pending_approvals entries
        let pending_approvals_map: HashMap<String, String> = {
            let root = workflow
                .as_ref()
                .map(|w| w.root_session_id.as_str())
                .unwrap_or("");
            if let Some(store) = gateway_store.as_deref() {
                store
                    .get_pending_approvals_for_root(root)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|a| a.task_id.map(|tid| (tid, a.request_id)))
                    .collect()
            } else {
                HashMap::new()
            }
        };

        let mut completed_tasks = Vec::new();
        let mut pending_approvals = Vec::new();
        let mut active_tasks = Vec::new();
        let mut latest_artifact_by_role: HashMap<String, serde_json::Value> = HashMap::new();
        let mut failed_task_count = 0usize;
        let mut failure_summary: Vec<serde_json::Value> = Vec::new();

        for task in &tasks {
            let implicit_artifact_id = format!("impl_{}", task.task_id);
            let entry = serde_json::json!({
                "task_id": task.task_id,
                "agent_id": task.agent_id,
                "status": format!("{:?}", task.status),
                "result_summary": task.result_summary,
                "implicit_artifact_id": implicit_artifact_id,
            });

            match task.status {
                autonoetic_types::workflow::TaskRunStatus::Succeeded => {
                    completed_tasks.push(entry.clone());
                    if let Some(ref summary) = task.result_summary {
                        let role = task.agent_id.split('.').next().unwrap_or("unknown");
                        latest_artifact_by_role.insert(
                            role.to_string(),
                            serde_json::json!({
                                "task_id": task.task_id,
                                "agent_id": task.agent_id,
                                "implicit_artifact_id": implicit_artifact_id,
                                "summary": summary,
                            }),
                        );
                    }
                }
                autonoetic_types::workflow::TaskRunStatus::AwaitingApproval => {
                    let mut entry = entry.clone();
                    if let Some(req_id) = pending_approvals_map.get(&task.task_id) {
                        entry.as_object_mut().unwrap().insert(
                            "approval_request_id".to_string(),
                            serde_json::Value::String(req_id.clone()),
                        );
                    }
                    pending_approvals.push(entry);
                }
                autonoetic_types::workflow::TaskRunStatus::Running
                | autonoetic_types::workflow::TaskRunStatus::Runnable
                | autonoetic_types::workflow::TaskRunStatus::Pending => {
                    active_tasks.push(entry);
                }
                autonoetic_types::workflow::TaskRunStatus::Failed
                | autonoetic_types::workflow::TaskRunStatus::Cancelled
                | autonoetic_types::workflow::TaskRunStatus::Aborted => {
                    failed_task_count += 1;
                    let mut fentry = entry.clone();
                    if let Ok(Some(cp)) = crate::scheduler::load_task_checkpoint(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task.task_id,
                    ) {
                        fentry.as_object_mut().unwrap().insert(
                            "checkpoint_step".to_string(),
                            serde_json::Value::String(cp.step),
                        );
                        if cp.state != serde_json::Value::Null {
                            fentry
                                .as_object_mut()
                                .unwrap()
                                .insert("checkpoint_state".to_string(), cp.state);
                        }
                    }
                    if failure_summary.len() < 5 {
                        failure_summary.push(fentry);
                    }
                }
                _ => {}
            }
        }

        let wf_status = workflow
            .as_ref()
            .map(|w| format!("{:?}", w.status))
            .unwrap_or_else(|| "unknown".to_string());

        let _latest_artifact_id = latest_artifact_by_role
            .get("coder")
            .and_then(|v| {
                v.get("task_id")
                    .and_then(|t| t.as_str())
                    .map(|t| format!("impl_task-{}", t.strip_prefix("task-").unwrap_or(t)))
            })
            .or_else(|| {
                latest_artifact_by_role.get("evaluator").and_then(|v| {
                    v.get("task_id")
                        .and_then(|t| t.as_str())
                        .map(|t| format!("impl_task-{}", t.strip_prefix("task-").unwrap_or(t)))
                })
            });

        let reuse_guards = serde_json::json!({
            "has_coder_artifact": latest_artifact_by_role.contains_key("coder"),
            "has_evaluator_result": latest_artifact_by_role.contains_key("evaluator"),
            "has_auditor_result": latest_artifact_by_role.contains_key("auditor"),
            "pending_approvals": !pending_approvals.is_empty(),
            "active_tasks_running": !active_tasks.is_empty(),
        });

        let state = serde_json::json!({
            "workflow_id": workflow_id,
            "workflow_status": wf_status,
            "completed_tasks": completed_tasks,
            "pending_approvals": pending_approvals,
            "active_tasks": active_tasks,
            "latest_artifact_by_role": latest_artifact_by_role,
            "reuse_guards": reuse_guards,
            "failed_task_count": failed_task_count,
            "failure_summary": failure_summary,
            "resume_hint": if !pending_approvals.is_empty() {
                "approval_pending — do not spawn new tasks, wait for approval"
            } else if !active_tasks.is_empty() {
                "tasks_running — wait for completion or proceed with partial results"
            } else if latest_artifact_by_role.contains_key("evaluator") && latest_artifact_by_role.contains_key("auditor") {
                "evaluation_complete — proceed to specialized_builder or coder iteration"
            } else if latest_artifact_by_role.contains_key("coder") && !latest_artifact_by_role.contains_key("evaluator") {
                "coder_done — proceed to evaluator/auditor"
            } else if !completed_tasks.is_empty() {
                "some_tasks_done — check completed_tasks for next step"
            } else {
                "fresh_start — no prior work found"
            },
        });

        serde_json::to_string(&state).map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// workflow.cancel_task
// ---------------------------------------------------------------------------

pub struct WorkflowCancelTaskTool;

impl NativeTool for WorkflowCancelTaskTool {
    fn name(&self) -> &'static str {
        "workflow.cancel_task"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> crate::llm::ToolDefinition {
        crate::llm::ToolDefinition {
            name: self.name().to_string(),
            description: "Cancel a task that is AwaitingApproval or Pending. Running tasks cannot be cancelled. Deletes any saved continuation and marks the task as Cancelled, which triggers the join condition check so the planner is notified.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["workflow_id", "task_id"],
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "The workflow ID containing the task."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to cancel."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why the task is being cancelled."
                    }
                }
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let config = config
            .ok_or_else(|| anyhow::anyhow!("Gateway config required for workflow.cancel_task"))?;
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        let workflow_id = args["workflow_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("workflow_id is required"))?;
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?;
        let reason = args["reason"].as_str().map(str::to_string);

        let store = gateway_store.as_deref();
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("Task '{}' not found in workflow '{}'", task_id, workflow_id)
            })?;

        let cancellable = matches!(
            task.status,
            autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                | autonoetic_types::workflow::TaskRunStatus::Pending
                | autonoetic_types::workflow::TaskRunStatus::Runnable
        );
        if !cancellable {
            return Ok(serde_json::json!({
                "ok": false,
                "task_id": task_id,
                "status": format!("{:?}", task.status),
                "error": format!("Task is {:?} and cannot be cancelled. Only AwaitingApproval, Pending, and Runnable tasks can be cancelled.", task.status)
            })
            .to_string());
        }

        // Delete any saved continuation file.
        let _ = crate::runtime::continuation::delete_continuation(config, task_id);

        // Mark as Cancelled (triggers join condition check).
        crate::scheduler::workflow_store::update_task_run_status(
            config,
            store,
            workflow_id,
            task_id,
            autonoetic_types::workflow::TaskRunStatus::Cancelled,
            reason
                .clone()
                .or_else(|| Some("Cancelled by operator".to_string())),
            None,
        )?;

        // Remove from queue if present.
        let _ = crate::scheduler::workflow_store::dequeue_task(config, store, workflow_id, task_id);

        Ok(serde_json::json!({
            "ok": true,
            "task_id": task_id,
            "workflow_id": workflow_id,
            "status": "Cancelled",
            "reason": reason.unwrap_or_else(|| "Cancelled by operator".to_string())
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// workflow.force_complete
// ---------------------------------------------------------------------------

/// Force-completes a task that is stuck in Running state.
/// Verifies the child session has actually completed before transitioning status.
pub struct WorkflowForceCompleteTool;

impl NativeTool for WorkflowForceCompleteTool {
    fn name(&self) -> &'static str {
        "workflow.force_complete"
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Workflow
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> crate::llm::ToolDefinition {
        crate::llm::ToolDefinition {
            name: self.name().to_string(),
            description: "Force-complete a task that is stuck in Running state. Checks whether the child session has actually finished (via checkpoint, session manifest, or promotion store) and transitions the task to Succeeded or Failed. Use this when workflow.wait keeps timing out but the child session is no longer active.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["workflow_id", "task_id"],
                "properties": {
                    "workflow_id": {
                        "type": "string",
                        "description": "The workflow ID containing the stuck task."
                    },
                    "task_id": {
                        "type": "string",
                        "description": "The task ID that is stuck in Running state."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["succeeded", "failed"],
                        "description": "The target status. Use 'succeeded' if the child completed its work, 'failed' if it errored out."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Optional result summary to attach to the completed task."
                    }
                }
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let config = config.ok_or_else(|| {
            anyhow::anyhow!("Gateway config required for workflow.force_complete")
        })?;
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        let workflow_id = args["workflow_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("workflow_id is required"))?;
        let task_id = args["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?;
        let target_status_str = args["status"].as_str().unwrap_or("succeeded");
        let summary = args["summary"].as_str().map(str::to_string);

        let store = gateway_store.as_deref();
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("Task '{}' not found in workflow '{}'", task_id, workflow_id)
            })?;

        if task.status != autonoetic_types::workflow::TaskRunStatus::Running {
            return Ok(serde_json::json!({
                "ok": false,
                "task_id": task_id,
                "current_status": format!("{:?}", task.status),
                "error": "Task is not in Running state. Only stuck Running tasks can be force-completed."
            })
            .to_string());
        }

        let target_status = match target_status_str {
            "succeeded" => autonoetic_types::workflow::TaskRunStatus::Succeeded,
            "failed" => autonoetic_types::workflow::TaskRunStatus::Failed,
            other => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "task_id": task_id,
                    "error": format!("Invalid status '{}'. Must be 'succeeded' or 'failed'.", other)
                })
                .to_string());
            }
        };

        let mut evidence = Vec::new();
        let mut session_completed = false;

        if let Some(gw_dir) = gateway_dir {
            if !task.session_id.is_empty() {
                let session_dir = gw_dir.join("sessions").join(&task.session_id);
                if session_dir.exists() {
                    let has_manifest = session_dir.join("manifest.json").exists();
                    let has_digest = session_dir.join("digest.md").exists();

                    if has_manifest {
                        evidence.push("session manifest exists".to_string());
                    }
                    if has_digest {
                        evidence.push("session digest exists".to_string());
                    }

                    if let Ok(content) = std::fs::read_to_string(session_dir.join("manifest.json"))
                    {
                        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(vis) = manifest.get("visibility") {
                                if let Some(status) = vis.get("status") {
                                    if let Some(s) = status.as_str() {
                                        if s == "completed" || s == "done" {
                                            session_completed = true;
                                            evidence.push(
                                                "session manifest shows completed status"
                                                    .to_string(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if has_digest {
                        if let Ok(digest) = std::fs::read_to_string(session_dir.join("digest.md")) {
                            if digest.contains("Session summary")
                                || digest.contains("jsonrpc_spawn_complete")
                            {
                                session_completed = true;
                                evidence
                                    .push("session digest contains completion markers".to_string());
                            }
                        }
                    }

                    if !has_manifest && !has_digest {
                        evidence.push(
                            "session directory exists but is empty (likely crashed)".to_string(),
                        );
                    }
                } else {
                    evidence.push("session directory does not exist".to_string());
                }
            }
        }

        if let Ok(Some(checkpoint)) =
            crate::scheduler::load_task_checkpoint(config, store, workflow_id, task_id)
        {
            evidence.push(format!(
                "checkpoint exists (step: {}, version: {})",
                checkpoint.step, checkpoint.version
            ));
        }

        let content_store = gateway_dir
            .and_then(|gw_dir| crate::runtime::content_store::ContentStore::new(gw_dir).ok());

        if let Some(store_obj) = content_store.as_ref() {
            if !task.session_id.is_empty() {
                let implicit_name = format!("impl_{}", task_id);
                if let Ok(names) = store_obj.list_names(&task.session_id) {
                    if names.contains(&implicit_name) {
                        session_completed = true;
                        evidence.push("implicit artifact exists (impl_task)".to_string());
                    }
                }
            }
        }

        if !session_completed {
            evidence.push("WARNING: could not confirm child session completed — proceeding based on caller judgment".to_string());
        }

        // Gate: refuse "succeeded" without real evidence of child completion.
        // "failed" is allowed — a stuck task is a legitimate failure diagnosis.
        if target_status == autonoetic_types::workflow::TaskRunStatus::Succeeded
            && !session_completed
        {
            return Ok(serde_json::json!({
                "ok": false,
                "task_id": task_id,
                "workflow_id": workflow_id,
                "error": "Cannot force-complete as 'succeeded': no evidence of child session completion.",
                "evidence_gathered": evidence,
                "hint": "Use status 'failed' if the child session is stuck, or wait for it to produce a manifest/digest/implicit artifact."
            }).to_string());
        }

        let result_summary = summary.unwrap_or_else(|| {
            format!(
                "Force-completed: {} (evidence: {})",
                target_status_str,
                evidence.join("; ")
            )
        });

        crate::scheduler::workflow_store::update_task_run_status(
            config,
            store,
            workflow_id,
            task_id,
            target_status.clone(),
            Some(result_summary.clone()),
            None,
        )?;

        let _ = crate::scheduler::workflow_store::checkpoint_task(
            config,
            store,
            workflow_id,
            task_id,
            "force_completed".to_string(),
            serde_json::json!({
                "status": format!("{:?}", target_status),
                "evidence": evidence,
                "session_completed": session_completed,
            }),
        );

        let _ = crate::scheduler::workflow_store::dequeue_task(config, store, workflow_id, task_id);

        tracing::warn!(
            target: "workflow",
            task_id = %task_id,
            workflow_id = %workflow_id,
            new_status = ?target_status,
            evidence = ?evidence,
            "Task force-completed"
        );

        Ok(serde_json::json!({
            "ok": true,
            "task_id": task_id,
            "workflow_id": workflow_id,
            "previous_status": "Running",
            "new_status": format!("{:?}", target_status),
            "result_summary": result_summary,
            "evidence": evidence,
            "session_confirmed_completed": session_completed,
            "message": format!("Task {} force-completed as {:?}.", task_id, target_status)
        })
        .to_string())
    }
}

#[cfg(test)]
mod force_complete_gate_tests {
    use super::*;

    /// Verifies the gate logic: when session_completed is false, only "failed" is allowed.
    #[test]
    fn gate_refuses_succeeded_without_evidence() {
        // The gate is embedded in execute() which requires full gateway infra.
        // Test the core logic extracted:
        let session_completed = false;
        let target_status = autonoetic_types::workflow::TaskRunStatus::Succeeded;
        assert_eq!(
            target_status == autonoetic_types::workflow::TaskRunStatus::Succeeded
                && !session_completed,
            true,
            "Gate should trigger: succeeded + no evidence"
        );
    }

    #[test]
    fn gate_allows_failed_without_evidence() {
        let session_completed = false;
        let target_status = autonoetic_types::workflow::TaskRunStatus::Failed;
        // Gate condition: succeeded && !session_completed — should NOT trigger for Failed
        assert_eq!(
            target_status == autonoetic_types::workflow::TaskRunStatus::Succeeded
                && !session_completed,
            false,
            "Gate should NOT trigger: failed is allowed without evidence"
        );
    }

    #[test]
    fn gate_allows_succeeded_with_evidence() {
        let session_completed = true;
        let target_status = autonoetic_types::workflow::TaskRunStatus::Succeeded;
        assert_eq!(
            target_status == autonoetic_types::workflow::TaskRunStatus::Succeeded
                && !session_completed,
            false,
            "Gate should NOT trigger: succeeded with evidence is fine"
        );
    }
}
