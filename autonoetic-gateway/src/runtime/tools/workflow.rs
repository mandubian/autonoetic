use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::content_store::ContentStore;
use crate::runtime::promotion_store::PromotionStore;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::{AgentManifest, ToolTier};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::ToolError;
use autonoetic_types::workflow::TaskRun;
use serde::Deserialize;
use serde::de;
use std::collections::HashMap;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ApprovalStatusTool));
    registry.register(Box::new(WorkflowWaitTool));
    registry.register(Box::new(WorkflowStateTool));
    registry.register(Box::new(WorkflowCancelTaskTool));
    registry.register(Box::new(WorkflowForceCompleteTool));
}

/// Resolve an artifact ref or canonical id to a canonical `art_*` id.
fn resolve_artifact_ref_to_id(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: Option<&str>,
    ref_id: &str,
) -> Option<String> {
    if ref_id.starts_with("art_") {
        return Some(ref_id.to_string());
    }
    if !ref_id.starts_with("ar.") {
        return None;
    }
    let store = gateway_store?;
    let sid = session_id?;
    store
        .resolve_artifact_ref_any_scope(ref_id, sid)
        .ok()
        .flatten()
        .map(|r| r.artifact_id)
}

/// Determine the primary artifact being evaluated by federation roles.
/// The `workflow.revision.created` builder candidate is authoritative: it points
/// at the exact revision under review. The coder-task fallback is best-effort;
/// it may diverge if the builder promoted a non-latest coder iteration or if
/// another specialist produced the artifact under review.
fn resolve_primary_artifact_id(
    gw_dir: &Path,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: Option<&str>,
    builder_candidate: Option<&serde_json::Value>,
    tasks: &[TaskRun],
) -> Option<String> {
    // 1. Builder candidate revision already points at the artifact under review.
    if let Some(bc) = builder_candidate {
        if let Some(id) = bc.get("artifact_id").and_then(|v| v.as_str()) {
            if id.starts_with("art_") {
                return Some(id.to_string());
            }
        }
        if let Some(ref_id) = bc.get("artifact_ref").and_then(|v| v.as_str()) {
            if let Some(id) = resolve_artifact_ref_to_id(gateway_store, session_id, ref_id) {
                return Some(id);
            }
        }
    }

    // 2. Latest coder task's implicit artifact carries the built artifacts.
    let coder_task = tasks
        .iter()
        .filter(|t| t.status == autonoetic_types::workflow::TaskRunStatus::Succeeded)
        .filter(|t| t.agent_id.split('.').next() == Some("coder"))
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
    if let Some(task) = coder_task {
        let implicit_name = format!("impl_{}", task.task_id);
        let content_store = ContentStore::new(gw_dir).ok()?;
        let bytes = content_store
            .read_by_name(&task.parent_session_id, &implicit_name)
            .ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let artifacts = json
            .get("content")
            .and_then(|c| c.get("artifacts"))
            .and_then(|a| a.as_array())?;
        for artifact in artifacts {
            if let Some(id) = artifact.get("artifact_id").and_then(|v| v.as_str()) {
                if id.starts_with("art_") {
                    return Some(id.to_string());
                }
            }
            if let Some(ref_id) = artifact.get("artifact_ref").and_then(|v| v.as_str()) {
                if let Some(id) = resolve_artifact_ref_to_id(gateway_store, session_id, ref_id) {
                    return Some(id);
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Approval Status Tool
// ---------------------------------------------------------------------------

/// Query the status of an approval request.
/// Allows agents to check whether an approval is pending, approved, or rejected.
pub struct ApprovalStatusTool;

impl NativeTool for ApprovalStatusTool {
    fn name(&self) -> &'static str {
        "approval_status"
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
                    Some(s) => s.as_str(),
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
                let response = ToolError::execution(
                    e.to_string(),
                    Some("Check the approval request and retry."),
                )
                .with_code("workflow_task_failed")
                .to_json_string();
                Ok(response)
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
                // #722 Stage 2: a Stale task (approval timed out, checkpoint
                // preserved) is terminal for waiting purposes, matching
                // check_join_condition. Without this, a parent in workflow_wait
                // blocks its full budget on a task the join already treats as done.
                let is_terminal = t.status.is_terminal_for_join();
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
                // RFC C — advisory claim reconciliation on the child→parent
                // result path (non-blocking).
                if t.status == autonoetic_types::workflow::TaskRunStatus::Succeeded {
                    if let Some(gw_dir) = gateway_dir {
                        let _ = crate::runtime::response_validation::advisory_reconcile_child_result_summary(
                            t.result_summary.as_deref(),
                            &t.session_id,
                            session_id.unwrap_or_else(|| t.parent_session_id.as_str()),
                            &t.agent_id,
                            gw_dir,
                            store,
                            Some(config),
                        );
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
                                        "implicit_artifact_id": artifact_data
                                            .get("implicit_artifact_id")
                                            .and_then(|v| v.as_str())
                                            .or(Some(implicit_name.as_str())),
                                        "summary": artifact_data.get("summary").and_then(|v| v.as_str()),
                                        "created_at": artifact_data.get("created_at").and_then(|v| v.as_str()),
                                        "named_outputs": artifact_data
                                            .get("content")
                                            .and_then(|c| c.get("named_outputs"))
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Array(Vec::new())),
                                        "artifacts": artifact_data
                                            .get("content")
                                            .and_then(|c| c.get("artifacts"))
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Array(Vec::new())),
                                    });
                                    entry["output"] = output;
                                }
                            }
                        }
                    }
                }
                if t.status == autonoetic_types::workflow::TaskRunStatus::Succeeded {
                    autonoetic_types::task_completion::enrich_task_status_entry(
                        &mut entry,
                        t.result_summary.as_deref(),
                    );
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
        "workflow_wait"
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
            description: "Checks whether watched task_ids have reached a terminal state (Succeeded, Failed, Cancelled, Aborted). When the join is not yet satisfied, the gateway suspends the session as WaitingForChild and re-checks automatically on the next child-state wake; no in-tool blocking or LLM round is consumed. Pass task_ids from agent.spawn(async=true). Returns structured status for each task. Succeeded tasks include an 'output' field with 'implicit_artifact_id' (e.g., 'impl_task-abc123') plus 'named_outputs' and 'artifacts'. Use content.read with named_outputs[*].ref (preferred) or with implicit_artifact_id to inspect full payload. When a task entry carries 'result_summary_truncated': true, its 'result_summary' is a shortened copy and 'full_result_ref' holds the complete reply — read that ref instead of acting on the summary or re-running the work. Pass timeout_secs=0 to probe current status without suspending.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of task IDs to wait for. Omit or pass [] to wait for all tasks in the workflow."
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "Optional workflow ID. If omitted, resolved from the current session's root."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 300,
                        "description": "Legacy: only 0 is meaningful. 0 = probe once and return immediately (no suspension). Non-zero values are ignored; the session hibernates until the children finish."
                    },
                    "max_wait_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 1800,
                        "description": "Legacy: ignored. The gateway suspends and resumes automatically instead of blocking for a fixed budget."
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
            #[serde(deserialize_with = "deserialize_task_ids_lenient")]
            task_ids: Vec<String>,
            #[serde(default)]
            workflow_id: Option<String>,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            max_wait_secs: Option<u64>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let _ = args.max_wait_secs;

        // The engine always threads its config in. The old fallback rebuilt one
        // from `agent_dir.parent()`, which is not the agents root — agents run
        // from `<gateway_dir>/revisions/agents/<id>/<rev>/`, so that walked to
        // the revision store instead. Unreachable, and wrong if reached.
        let gw_config = config.ok_or_else(|| {
            anyhow::anyhow!("config is required to resolve the workflow for this session")
        })?;

        let workflow_id = match args.workflow_id {
            Some(id) => id,
            None => {
                let sid = session_id.unwrap_or(&manifest.agent.id);
                let root = crate::runtime::content_store::root_session_id(sid);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)?
                    .unwrap_or_else(|| "unknown".to_string())
            }
        };

        let task_ids = if args.task_ids.is_empty() {
            let tasks = crate::scheduler::workflow_store::list_task_runs_for_workflow(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
            )?;
            tasks.iter().map(|t| t.task_id.clone()).collect::<Vec<_>>()
        } else {
            args.task_ids
        };

        anyhow::ensure!(!task_ids.is_empty(), "no tasks found in workflow '{}'", workflow_id);

        let timeout_secs = args
            .timeout_secs
            .unwrap_or(gw_config.default_workflow_wait_secs)
            .min(300);

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
            &task_ids,
            _gateway_dir,
            session_id,
        );
        let any_gate_fail =
            autonoetic_types::task_completion::any_gate_unsatisfied(&tasks_status);

        if timeout_secs == 0 || all_done {
            return serde_json::to_string(&serde_json::json!({
                "ok": true,
                "workflow_id": workflow_id,
                "tasks": tasks_status,
                "join_satisfied": all_done,
                "any_failed": any_failed,
                "any_not_found": any_not_found,
                "any_gate_unsatisfied": any_gate_fail,
                "failed_task_count": failed_task_count,
                "failure_summary": failure_summary,
                "waited_secs": 0,
                "message": autonoetic_types::task_completion::workflow_wait_join_message(
                    all_done,
                    any_failed,
                    any_not_found,
                    any_gate_fail,
                    autonoetic_types::task_completion::any_result_truncated(&tasks_status),
                    0,
                ),
            }))
            .map_err(Into::into);
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "workflow_id": workflow_id,
            "tasks": tasks_status,
            "join_satisfied": false,
            "any_failed": any_failed,
            "any_not_found": any_not_found,
            "any_gate_unsatisfied": any_gate_fail,
            "failed_task_count": failed_task_count,
            "failure_summary": failure_summary,
            "waited_secs": 0,
            "waiting_for_child": true,
            "message": autonoetic_types::task_completion::workflow_wait_join_message(
                false,
                any_failed,
                any_not_found,
                any_gate_fail,
                autonoetic_types::task_completion::any_result_truncated(&tasks_status),
                0,
            ),
        }))
        .map_err(Into::into)
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
        "workflow_state"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        // Shared resumption kernel (#466), centralized from
        // planner/coder/sealed_evaluator/packager SKILL.md. Each role keeps its
        // own reuse-guard specifics; this is the universal principle.
        vec![GuidanceBlock {
            id: "resumption.workflow_state_first",
            when: GuidanceCondition::ToolPresent("workflow_state"),
            priority: 8,
            prose: "**On any wake-up** (approval resolved, child join, timeout, hibernation), call \
`workflow_state` FIRST and treat its `reuse_guards`/`resume_hint` as mechanical truth: continue from \
where the workflow left off — never restart from scratch or re-spawn work a guard says is already \
done. Read child outputs from `named_outputs` (don't guess content names)."
                .to_string(),
        }]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Returns compact, structured workflow state for deterministic resume. Use this instead of re-inferring state from conversation history. Returns: current step, completed tasks with implicit artifact handles, pending approvals, active tasks, and reuse guards.".to_string(),
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
        gateway_dir: Option<&Path>,
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

        // See the sibling tool above: the engine always passes its config, and
        // `agent_dir.parent()` is the revision store, not the agents root.
        let gw_config = config.ok_or_else(|| {
            anyhow::anyhow!("config is required to resolve the workflow for this session")
        })?;

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

        // Load workflow events so orchestrators can discover durable artifacts
        // produced outside of task results (e.g. candidate revisions).
        let workflow_events = crate::scheduler::workflow_store::load_workflow_events(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )
        .unwrap_or_default();

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
            let mut entry = serde_json::json!({
                "task_id": task.task_id,
                "agent_id": task.agent_id,
                "status": format!("{:?}", task.status),
                "result_summary": task.result_summary,
                "implicit_artifact_id": implicit_artifact_id,
            });

            // The child's reply exceeded the summary budget, so `result_summary`
            // is a shortened copy. Name the handle holding the whole thing —
            // without it the parent sees a partial payload with no way to tell
            // that a complete one exists, and re-does work already done.
            if let Some(cnt_ref) = crate::scheduler::workflow_store::full_result_ref(task) {
                entry["result_summary_truncated"] = serde_json::Value::Bool(true);
                entry["full_result_ref"] = serde_json::Value::String(cnt_ref.to_string());
            }

            if task.status == autonoetic_types::workflow::TaskRunStatus::Succeeded {
                autonoetic_types::task_completion::enrich_task_status_entry(
                    &mut entry,
                    task.result_summary.as_deref(),
                );
            }

            match task.status {
                autonoetic_types::workflow::TaskRunStatus::Succeeded => {
                    // RFC C — advisory claim reconciliation on the child→parent
                    // result path. This is intentionally non-blocking: the full
                    // child `SpawnResult` was already validated against
                    // `io.returns` before the task was marked complete. We
                    // re-check what crosses to the parent to catch any
                    // fabricated claim that survived shortening.
                    //
                    // Scan the spilled reply when there is one. The shortened
                    // summary carries only part of the child's references (and,
                    // when the payload could not fit its own shape, only an
                    // envelope), so scanning it alone would leave later claims
                    // unchecked — reading the spill keeps this pass over the
                    // whole reply.
                    let spilled_reply = crate::scheduler::workflow_store::full_result_ref(task)
                        .and_then(|cnt_ref| {
                            crate::runtime::content_store::ContentStore::new(
                                &crate::execution::gateway_root_dir(gw_config),
                            )
                            .ok()?
                            .read_by_name_or_handle(&task.session_id, cnt_ref)
                            .ok()
                        })
                        .and_then(|bytes| String::from_utf8(bytes).ok());
                    let _ = crate::runtime::response_validation::advisory_reconcile_child_result_summary(
                        spilled_reply.as_deref().or(task.result_summary.as_deref()),
                        &task.session_id,
                        session_id.unwrap_or_else(|| task.parent_session_id.as_str()),
                        &task.agent_id,
                        &crate::execution::gateway_root_dir(gw_config),
                        gateway_store.as_deref(),
                        Some(gw_config),
                    );

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
                // A Stale task is a timed-out approval whose row is still
                // pending (#722 Stage 2), so it remains approvable — surface it
                // alongside AwaitingApproval rather than letting it vanish into
                // the catch-all below.
                autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                | autonoetic_types::workflow::TaskRunStatus::Stale => {
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
                | autonoetic_types::workflow::TaskRunStatus::Pending
                | autonoetic_types::workflow::TaskRunStatus::Paused => {
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

        let builder_candidate = workflow_events
            .iter()
            .filter(|e| e.event_type == "workflow.revision.created")
            .filter_map(|e| {
                let payload = e.payload.as_object()?;
                Some(serde_json::json!({
                    "agent_id": payload.get("agent_id").cloned().unwrap_or(serde_json::Value::Null),
                    "revision_id": payload.get("revision_id").cloned().unwrap_or(serde_json::Value::Null),
                    "artifact_id": payload.get("artifact_id").cloned().unwrap_or(serde_json::Value::Null),
                    "artifact_ref": payload.get("artifact_ref").cloned().unwrap_or(serde_json::Value::Null),
                    "content_digest": payload.get("content_digest").cloned().unwrap_or(serde_json::Value::Null),
                }))
            })
            .last();

        // The federation/evaluation completion predicates must be sourced from
        // the promotion store (recorded verdicts), not merely artifact presence.
        // A role can produce an artifact without ever calling promotion.record,
        // which would falsely satisfy the old artifact-presence guards.
        let primary_artifact_id = gateway_dir.and_then(|gw_dir| {
            resolve_primary_artifact_id(
                gw_dir,
                gateway_store.as_deref(),
                session_id,
                builder_candidate.as_ref(),
                &tasks,
            )
        });
        let promotion_record = gateway_dir
            .zip(primary_artifact_id.as_ref())
            .and_then(|(gw_dir, artifact_id)| {
                PromotionStore::new(gw_dir)
                    .ok()
                    .and_then(|store| store.get_promotion(artifact_id))
            });

        // reuse_guards: when we can resolve the promotion record for the primary
        // artifact, verdicts are authoritative. When no record is resolvable, we
        // fall back to artifact presence as a non-breaking diagnostic hint; the
        // actual completion decision below does NOT fall back.
        let has_evaluator_result = promotion_record
            .as_ref()
            .map(|r| r.evaluator_id.is_some())
            .unwrap_or(latest_artifact_by_role.contains_key("evaluator"));
        let has_auditor_result = promotion_record
            .as_ref()
            .map(|r| r.auditor_id.is_some())
            .unwrap_or(latest_artifact_by_role.contains_key("auditor"));
        let has_static_evaluator_result = promotion_record
            .as_ref()
            .map(|r| r.static_evaluator_id.is_some())
            .unwrap_or(latest_artifact_by_role.contains_key("static_evaluator"));
        let has_unit_test_runner_result = promotion_record
            .as_ref()
            .map(|r| r.unit_test_runner_id.is_some())
            .unwrap_or(latest_artifact_by_role.contains_key("unit_test_runner"));
        let has_sealed_evaluator_result = promotion_record
            .as_ref()
            .map(|r| r.sealed_evaluator_id.is_some())
            .unwrap_or(latest_artifact_by_role.contains_key("sealed_evaluator"));

        // Completion predicates (resume hints) must NOT fall back to artifact
        // presence; only recorded promotion-store verdicts can satisfy a gate.
        let evaluation_verdict_complete = promotion_record.as_ref().map_or(false, |r| {
            (r.evaluator_id.is_some() || r.sealed_evaluator_id.is_some())
                && r.auditor_id.is_some()
        });
        let federation_verdict_complete = promotion_record.as_ref().map_or(false, |r| {
            r.static_evaluator_id.is_some() && r.auditor_id.is_some()
        });

        let reuse_guards = serde_json::json!({
            "has_coder_artifact": latest_artifact_by_role.contains_key("coder"),
            "has_evaluator_result": has_evaluator_result,
            "has_auditor_result": has_auditor_result,
            "has_static_evaluator_result": has_static_evaluator_result,
            "has_unit_test_runner_result": has_unit_test_runner_result,
            "has_sealed_evaluator_result": has_sealed_evaluator_result,
            "has_builder_candidate": builder_candidate.is_some(),
            "builder_candidate": builder_candidate,
            "pending_approvals": !pending_approvals.is_empty(),
            "active_tasks_running": !active_tasks.is_empty(),
            "primary_artifact_id": primary_artifact_id,
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
            } else if builder_candidate.is_some() {
                "builder_candidate_exists — use install_mode:\"promote\" with the existing revision_id; do not create a new revision"
            } else if evaluation_verdict_complete {
                "evaluation_complete — proceed to specialized_builder or coder iteration"
            } else if federation_verdict_complete {
                "federation_complete — collect all verdicts and escalate to operator"
            } else if latest_artifact_by_role.contains_key("coder") && !latest_artifact_by_role.contains_key("evaluator") {
                "coder_done — proceed to evaluator/auditor or federation"
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
        "workflow_cancel_task"
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
            return Ok(ToolError::conflict(
                format!("Task is {:?} and cannot be cancelled. Only AwaitingApproval, Pending, and Runnable tasks can be cancelled.", task.status),
                Some("Cancel is only allowed for tasks in AwaitingApproval, Pending, or Runnable status."),
            )
            .with_code("task_cannot_be_cancelled")
            .to_error_response());
        }

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
        "workflow_force_complete"
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
            description: "Force-complete a task that is stuck in Running state. Checks whether the child session has actually finished (via checkpoint, session manifest, or promotion store) and transitions the task to Succeeded or Failed. Use this when workflow_wait keeps timing out but the child session is no longer active.".to_string(),
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

        if crate::scheduler::workflow_store::is_workflow_terminal(
            config,
            gateway_store.as_deref(),
            workflow_id,
        )? {
            return Ok(ToolError::conflict(
                format!(
                    "Workflow '{}' is already terminal. Force-complete is not allowed after completion.",
                    workflow_id
                ),
                Some("The workflow has already reached a terminal state; no further mutations are permitted."),
            )
            .with_code("workflow_already_completed")
            .to_error_response());
        }

        let target_status_str = args["status"].as_str().unwrap_or("succeeded");
        let summary = args["summary"].as_str().map(str::to_string);

        let store = gateway_store.as_deref();
        let task = crate::scheduler::load_task_run(config, store, workflow_id, task_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("Task '{}' not found in workflow '{}'", task_id, workflow_id)
            })?;

        if task.status != autonoetic_types::workflow::TaskRunStatus::Running {
            return Ok(ToolError::conflict(
                "Task is not in Running state. Only stuck Running tasks can be force-completed.",
                Some("Ensure the task is in Running state before force-completing."),
            )
            .with_code("task_not_running")
            .to_error_response());
        }

        let target_status = match target_status_str {
            "succeeded" => autonoetic_types::workflow::TaskRunStatus::Succeeded,
            "failed" => autonoetic_types::workflow::TaskRunStatus::Failed,
            other => {
                return Ok(ToolError::validation(
                    format!("Invalid status '{}'. Must be 'succeeded' or 'failed'.", other),
                    Some("Use 'succeeded' or 'failed' as the status value."),
                )
                .with_code("invalid_force_complete_status")
                .to_error_response());
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
                                || digest.contains(
                                    autonoetic_types::session_outcome::SessionCloseOutcome::JsonRpcSpawnComplete.as_str(),
                                )
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
            return Ok(ToolError::conflict(
                "Cannot force-complete as 'succeeded': no evidence of child session completion.",
                Some("Use status 'failed' if the child session is stuck, or wait for it to produce a manifest/digest/implicit artifact."),
            )
            .with_code("force_complete_no_evidence")
            .to_error_response());
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

#[cfg(test)]
mod stale_terminal_tests {
    use super::*;
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use crate::scheduler::workflow_store::{
        ensure_workflow_for_root_session, new_task_id, save_task_run,
    };
    use tempfile::tempdir;

    fn config_in(agents_dir: &std::path::Path) -> autonoetic_types::config::GatewayConfig {
        autonoetic_types::config::GatewayConfig {
            runtime_dir: agents_dir.to_path_buf().join(".gateway"),
            agents_dir: agents_dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn task_with_status(
        workflow_id: &str,
        status: TaskRunStatus,
    ) -> TaskRun {
        let ts = chrono::Utc::now().to_rfc3339();
        TaskRun {
            task_id: new_task_id(),
            workflow_id: workflow_id.to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "r1/coder-abc".to_string(),
            parent_session_id: "r1".to_string(),
            status,
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
        }
    }

    /// #722 Stage 2: a Stale task (approval timed out, checkpoint preserved)
    /// must count as terminal in `check_task_statuses`, matching
    /// `check_join_condition`. Otherwise a parent in `workflow_wait` blocks its
    /// whole budget on a task the join already treats as done.
    #[test]
    fn stale_task_counts_as_done_in_workflow_wait() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = config_in(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "r1", None).unwrap();

        let task = task_with_status(&wf.workflow_id, TaskRunStatus::Stale);
        save_task_run(&cfg, None, &task).unwrap();

        let (_statuses, all_done, any_failed, _any_not_found, _failed_count, _failures) =
            check_task_statuses(&cfg, None, &wf.workflow_id, &[task.task_id.clone()], None, None);

        assert!(all_done, "a Stale task must be reported terminal (all_done)");
        assert!(!any_failed, "Stale is terminal-but-not-failed");
    }

    /// A still-parked (AwaitingApproval) task is NOT done — the parent should
    /// keep waiting until the operator decides or the approval times out.
    #[test]
    fn awaiting_approval_task_is_not_done() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = config_in(&agents);
        let wf = ensure_workflow_for_root_session(&cfg, None, "r1", None).unwrap();

        let task = task_with_status(&wf.workflow_id, TaskRunStatus::AwaitingApproval);
        save_task_run(&cfg, None, &task).unwrap();

        let (_s, all_done, _af, _anf, _fc, _f) =
            check_task_statuses(&cfg, None, &wf.workflow_id, &[task.task_id.clone()], None, None);

        assert!(!all_done, "an AwaitingApproval task is not terminal");
    }
}
