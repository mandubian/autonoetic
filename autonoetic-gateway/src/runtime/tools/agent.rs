use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::continuation;
use crate::runtime::tools::{capability_type_name, validate_agent_id, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{GatewayConfig, SchemaEnforcementConfig, SchemaEnforcementMode};
use autonoetic_types::schema_enforcement::{default_enforcer, EnforcementResult, SchemaEnforcer};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowEventRecord};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    agent_id: String,
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    /// Optional bounded context summary from the parent. When provided,
    /// injected as a system message before the user message. The parent
    /// should summarize only what the child needs; the gateway does not
    /// automatically share the parent's full conversation history.
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// When true, enqueue the task for async execution and return immediately with task_id.
    /// The scheduler will execute the child agent in the background.
    /// Use workflow.wait to check task status.
    #[serde(default)]
    r#async: bool,
    /// Join group name. Tasks in the same join group are awaited together by the planner.
    #[serde(default)]
    join_group: Option<String>,
}

pub struct AgentSpawnTool;

impl NativeTool for AgentSpawnTool {
    fn name(&self) -> &'static str {
        "agent.spawn"
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
            description: "Delegate a task to a specialist agent. With async=false (default), blocks until the child completes and returns its reply. With async=true, returns immediately with a task_id — use workflow.wait to check status. Spawn multiple children in parallel with async=true, then wait for all of them.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "message": { "type": "string", "description": "The task to delegate. Should be self-contained; avoid dumping full conversation history." },
                    "context": { "type": "string", "description": "Optional bounded context summary. Include only what the child needs (goals, decisions, key facts, open items). The parent's full conversation history is NOT automatically shared." },
                    "metadata": { "type": "object" },
                    "session_id": { "type": "string" },
                    "async": { "type": "boolean", "description": "If true, enqueue for background execution and return immediately with task_id. Default: false (synchronous)." },
                    "join_group": { "type": "string", "description": "Optional group name for join semantics. Tasks in the same group are awaited together before planner resumes." }
                },
                "required": ["agent_id", "message"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: SpawnAgentArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");

        // Schema enforcement hook
        let default_enforcement_config = SchemaEnforcementConfig::default();
        let enforcement_config = config
            .map(|c| &c.schema_enforcement)
            .unwrap_or(&default_enforcement_config);

        if enforcement_config.mode != SchemaEnforcementMode::Disabled {
            let agents_dir = config
                .map(|c| &c.agents_dir)
                .ok_or_else(|| anyhow::anyhow!("config is required for agent.spawn schema enforcement"))?;
            let target_agent_path = agents_dir.join(&args.agent_id).join("SKILL.md");

            if target_agent_path.exists() {
                if let Ok(manifest_content) = std::fs::read_to_string(&target_agent_path) {
                    if let Some(frontmatter) = manifest_content.split("---").nth(1) {
                        if let Ok(target_manifest) =
                            serde_yaml::from_str::<AgentManifest>(frontmatter)
                        {
                            if let Some(io) = &target_manifest.io {
                                if let Some(accepts) = &io.accepts {
                                    let enforcer = default_enforcer();
                                    let payload = serde_json::json!({
                                        "message": args.message,
                                        "context": args.context,
                                        "metadata": args.metadata,
                                        "session_id": args.session_id,
                                    });

                                    match enforcer.enforce(&payload, accepts) {
                                        EnforcementResult::Reject(details) => {
                                            return Err(anyhow::anyhow!(
                                                "Schema validation failed: {}. Hint: {}",
                                                details.reason,
                                                details.hint.unwrap_or_default()
                                            ));
                                        }
                                        EnforcementResult::Coerced(details) => {
                                            if enforcement_config.audit {
                                                tracing::info!(
                                                    target: "schema_enforcement",
                                                    agent_id = %args.agent_id,
                                                    transformations = ?details.transformations,
                                                    "Schema enforcement: payload coerced"
                                                );
                                            }
                                        }
                                        EnforcementResult::Pass => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let resolved_session_id = args
            .session_id
            .clone()
            .or_else(|| session_id.map(ToOwned::to_owned))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let agents_dir = config
            .map(|c| &c.agents_dir)
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.spawn"))?;

        let fallback_gateway_config = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_gateway_config);

        // Role-agnostic: block synchronous delegation while any approval for this *root*
        // session is still pending. Async spawn (args.async) is NOT blocked — it queues
        // independently and the scheduler picks it up regardless of approval state.
        if !args.r#async {
            let root_for_approval_check =
                crate::runtime::content_store::root_session_id(&resolved_session_id);
            let pending = crate::scheduler::approval::pending_approval_requests_for_root(
                gw_config,
                gateway_store.as_deref(),
                &root_for_approval_check,
            )?;
            if !pending.is_empty() {
                let ids: Vec<String> = pending.iter().map(|r| r.request_id.clone()).collect();
                return Err(anyhow::anyhow!(
                    "Cannot delegate (agent.spawn) while approval(s) are pending for this session. Pending request id(s): {}. Approve or reject with `autonoetic gateway approvals approve|reject <id> --config <path>`, then continue.",
                    ids.join(", ")
                ));
            }
        }

        let root = crate::runtime::content_store::root_session_id(&resolved_session_id);
        let source_agent_id = manifest.agent.id.clone();
        let workflow = crate::scheduler::ensure_workflow_for_root_session(
            gw_config,
            gateway_store.as_deref(),
            &root,
            Some(source_agent_id.as_str()),
        )?;
        let workflow_id = workflow.workflow_id.clone();
        let task_id = crate::scheduler::new_task_id();

        let execution_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let execution =
            crate::execution::GatewayExecutionService::new(execution_config, gateway_store.clone());

        let target_agent_id = args.agent_id.clone();
        let kickoff_message = match (&args.context, &args.metadata) {
            (Some(ctx), Some(meta)) => {
                format!(
                    "[Context]\n{}\n\n[Task]\n{}\n\n[Metadata]\n{}",
                    ctx, args.message, meta
                )
            }
            (Some(ctx), None) => {
                format!("[Context]\n{}\n\n[Task]\n{}", ctx, args.message)
            }
            (None, Some(meta)) => {
                format!("{}\n\nDelegation metadata: {}", args.message, meta)
            }
            (None, None) => args.message.clone(),
        };

        // Set up hierarchical content namespace for the child agent
        // The child gets a unique delegation path (e.g., "demo-session-1/coder-abc123")
        // so content written by the child is visible to the parent via the hierarchy
        let child_delegation_path = format!(
            "{}/{}-{}",
            resolved_session_id,
            args.agent_id,
            &uuid::Uuid::new_v4().to_string()[..8]
        );

        let ts = Utc::now().to_rfc3339();
        let task = TaskRun {
            task_id: task_id.clone(),
            workflow_id: workflow_id.clone(),
            agent_id: target_agent_id.clone(),
            session_id: child_delegation_path.clone(),
            parent_session_id: resolved_session_id.clone(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts,
            source_agent_id: Some(source_agent_id.clone()),
            result_summary: None,
            join_group: None,
            message: Some(kickoff_message.clone()),
            metadata: args.metadata.clone(),
        };
        crate::scheduler::save_task_run(gw_config, gateway_store.as_deref(), &task)?;
        crate::scheduler::append_workflow_event(
            gw_config,
            gateway_store.as_deref(),
            &WorkflowEventRecord {
                event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                workflow_id: workflow_id.clone(),
                task_id: Some(task_id.clone()),
                event_type: "task.spawned".to_string(),
                agent_id: Some(target_agent_id.clone()),
                payload: serde_json::json!({
                    "target_agent_id": target_agent_id,
                    "child_session_id": child_delegation_path,
                    "parent_session_id": resolved_session_id,
                }),
                occurred_at: Utc::now().to_rfc3339(),
            },
        )?;

        crate::scheduler::workflow_causal::mirror_orchestration_event(
            gw_config,
            root,
            "workflow.task.spawned",
            autonoetic_types::causal_chain::EntryStatus::Success,
            serde_json::json!({
                "workflow_id": workflow_id,
                "task_id": task_id,
                "target_agent_id": target_agent_id,
                "child_session_id": child_delegation_path,
                "parent_session_id": resolved_session_id,
                "source_agent_id": source_agent_id,
            }),
        );

        // Set root session relationship so child's session-visible content is visible to parent
        // Must use the same gateway_dir as the execution engine, NOT agent_dir.parent()
        if let Some(gw_dir) = gateway_dir {
            if let Ok(store) = crate::runtime::content_store::ContentStore::new(gw_dir) {
                if let Err(e) = store.set_root_session(&child_delegation_path, root) {
                    tracing::warn!(
                        target: "content_store",
                        error = %e,
                        parent_session = %resolved_session_id,
                        child_delegation = %child_delegation_path,
                        "Failed to set root session for child agent"
                    );
                } else {
                    tracing::info!(
                        target: "content_store",
                        parent_session = %resolved_session_id,
                        child_delegation = %child_delegation_path,
                        "Set up hierarchical content namespace for child agent"
                    );
                }
            }
        }

        // --- Async branch: enqueue and return immediately ---
        if args.r#async {
            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task_id.clone(),
                workflow_id: workflow_id.clone(),
                agent_id: target_agent_id.clone(),
                message: kickoff_message,
                child_session_id: child_delegation_path.clone(),
                parent_session_id: resolved_session_id.clone(),
                source_agent_id: source_agent_id.clone(),
                metadata: args.metadata.clone(),
                join_group: args.join_group,
                blocks_planner: true,
                enqueued_at: Utc::now().to_rfc3339(),
            };
            crate::scheduler::enqueue_task(gw_config, gateway_store.as_deref(), &queued)?;

            // Update task status from Running → Pending (it's queued, not yet executing)
            let _ = crate::scheduler::update_task_run_status(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
                &task_id,
                TaskRunStatus::Pending,
                Some("queued".to_string()),
                None,
            );

            return serde_json::to_string(&serde_json::json!({
                "ok": true,
                "accepted": true,
                "status": "queued",
                "workflow_id": workflow_id,
                "task_id": task_id,
                "agent_id": target_agent_id,
                "session_id": child_delegation_path,
                "message": "Task queued for async execution. Use workflow.wait with task_ids to check completion status."
            }))
            .map_err(Into::into);
        }

        // --- Synchronous branch (existing behavior) ---
        let wf_id_clone = workflow_id.clone();
        let tid_clone = task_id.clone();
        let spawn_future = async move {
            execution
                .spawn_agent_once(
                    &target_agent_id,
                    &kickoff_message,
                    &child_delegation_path, // Use delegation path as session_id for content namespace
                    Some(&source_agent_id),
                    false,
                    None,
                    args.metadata.as_ref(),
                    Some(&wf_id_clone),
                    Some(&tid_clone),
                )
                .await
        };

        let spawn_result = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(spawn_future))
        } else {
            tokio::runtime::Runtime::new()?.block_on(spawn_future)
        };

        match spawn_result {
            Ok(result) => {
                if let Some(request_id) = &result.suspended_for_approval {
                    let summary = format!("awaiting approval {}", request_id);

                    // Load continuation to get approval details (tool name, etc.)
                    let approval_metadata = continuation::load_continuation(gw_config, &task_id)
                        .ok()
                        .flatten()
                        .and_then(|cont| {
                            let tool_name = cont.pending_tool_call.tool_name.clone();
                            // Derive approval kind from tool name
                            let kind = if tool_name.contains("sandbox") {
                                "sandbox".to_string()
                            } else if tool_name.contains("install") {
                                "agent_install".to_string()
                            } else {
                                "tool_execution".to_string()
                            };
                            // Try to extract reason from approval_response
                            let reason = serde_json::from_str::<serde_json::Value>(&cont.pending_tool_call.approval_response)
                                .ok()
                                .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from));

                            Some(crate::scheduler::ApprovalMetadata {
                                request_id: cont.approval_request_id,
                                kind,
                                reason,
                            })
                        });

                    if let Err(e) = crate::scheduler::update_task_run_status(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task_id,
                        TaskRunStatus::AwaitingApproval,
                        Some(summary),
                        approval_metadata,
                    ) {
                        tracing::warn!(
                            target: "workflow",
                            error = %e,
                            workflow_id = %workflow_id,
                            task_id = %task_id,
                            "Failed to persist task awaiting approval status"
                        );
                    }

                    let _ = crate::scheduler::checkpoint_task(
                        gw_config,
                        gateway_store.as_deref(),
                        &workflow_id,
                        &task_id,
                        "awaiting_approval".to_string(),
                        serde_json::json!({
                            "status": "awaiting_approval",
                            "approval_request_id": request_id,
                        }),
                    );

                    // Return success — the task is queued and will resume after approval.
                    // The planner should call workflow.wait to get the result.
                    // Do NOT expose awaiting_approval status — the planner doesn't need to know.
                    let summary = format!(
                        "Task queued: {} will execute after approval ({}). Call workflow.wait to get the result.",
                        result.agent_id, request_id
                    );
                    return Ok(serde_json::json!({
                        "ok": true,
                        "status": "queued",
                        "workflow_id": workflow_id,
                        "task_id": task_id,
                        "agent_id": result.agent_id,
                        "session_id": result.session_id,
                        "result_summary": summary,
                        "files": result.files,
                    })
                    .to_string());
                }

                let summary = result.assistant_reply.as_ref().map(|s| {
                    const MAX: usize = 512;
                    if s.len() <= MAX {
                        s.clone()
                    } else {
                        format!("{}…", &s[..MAX])
                    }
                });
                if let Err(e) = crate::scheduler::update_task_run_status(
                    gw_config,
                    gateway_store.as_deref(),
                    &workflow_id,
                    &task_id,
                    TaskRunStatus::Succeeded,
                    summary,
                    None,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %e,
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        "Failed to persist task completion status"
                    );
                }
                Ok(serde_json::json!({
                    "ok": true,
                    "status": "agent_spawned",
                    "workflow_id": workflow_id,
                    "task_id": task_id,
                    "agent_id": result.agent_id,
                    "session_id": result.session_id,
                    "assistant_reply": result.assistant_reply,
                    "artifacts": result.artifacts,
                    // All named content written by the child — use name/handle/alias with content.read
                    "files": result.files,
                    "shared_knowledge": result.shared_knowledge,
                    "llm_usage": result.llm_usage,
                })
                .to_string())
            }
            Err(e) => {
                if let Err(inner) = crate::scheduler::update_task_run_status(
                    gw_config,
                    gateway_store.as_deref(),
                    &workflow_id,
                    &task_id,
                    TaskRunStatus::Failed,
                    Some(e.to_string()),
                    None,
                ) {
                    tracing::warn!(
                        target: "workflow",
                        error = %inner,
                        workflow_id = %workflow_id,
                        task_id = %task_id,
                        "Failed to persist task failure status"
                    );
                }
                Err(e)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AgentExistsArgs {
    agent_id: String,
}

pub struct AgentExistsTool;

impl NativeTool for AgentExistsTool {
    fn name(&self) -> &'static str {
        "agent.exists"
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
            description: "Check if an agent with the given ID already exists in the repository (authoring tree or resolved revision paths).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The agent ID to check for existence" }
                },
                "required": ["agent_id"],
                "additionalProperties": false
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
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentExistsArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        validate_agent_id(&args.agent_id)?;

        let agents_dir = config
            .map(|c| &c.agents_dir)
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.exists"))?;

        let repo = crate::agent::AgentRepository::new(agents_dir.clone());

        match repo.get_sync(&args.agent_id) {
            Ok(_) => Ok(serde_json::json!({
                "ok": true,
                "exists": true,
                "agent_id": args.agent_id,
                "status": "healthy",
            })
            .to_string()),
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains("not found") {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": false,
                        "agent_id": args.agent_id,
                        "status": "not_found",
                    })
                    .to_string())
                } else if error_msg.contains("identity mismatch") {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": true,
                        "agent_id": args.agent_id,
                        "status": "identity_mismatch",
                        "error": error_msg,
                        "message": "Agent directory exists but manifest ID does not match directory name. This agent needs to be fixed before use."
                    })
                    .to_string())
                } else {
                    Ok(serde_json::json!({
                        "ok": true,
                        "exists": true,
                        "agent_id": args.agent_id,
                        "status": "load_error",
                        "error": error_msg,
                        "message": "Agent exists but cannot be loaded. Check SKILL.md syntax or file permissions."
                    })
                    .to_string())
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AgentDiscoverArgs {
    intent: String,
    #[serde(default)]
    required_capabilities: Vec<String>,
    #[serde(default)]
    exclude_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AgentDiscoveryResult {
    score: f64,
    agent_id: String,
    name: String,
    description: String,
    capabilities: Vec<String>,
    match_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    io: Option<serde_json::Value>,
}

pub struct AgentDiscoverTool;

impl NativeTool for AgentDiscoverTool {
    fn name(&self) -> &'static str {
        "agent.discover"
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
            description: "Discover existing agents that match the given intent and capabilities. Returns ranked candidates with match scores and reasons. Use this before deciding to install a new agent to prefer reuse over creation.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intent": { "type": "string", "description": "The task intent or goal to match against agent descriptions" },
                    "required_capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "List of required capability types (e.g., 'CodeExecution', 'WriteAccess', 'NetworkAccess')"
                    },
                    "exclude_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent IDs to exclude from results"
                    }
                },
                "required": ["intent"],
                "additionalProperties": false
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
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentDiscoverArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.intent.trim().is_empty(), "intent must not be empty");

        let agents_dir = config
            .map(|c| &c.agents_dir)
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.discover"))?;

        let repo = crate::agent::AgentRepository::new(agents_dir.clone());
        let loaded_agents = repo.list_loaded_sync()?;

        let mut results: Vec<AgentDiscoveryResult> = loaded_agents
            .into_iter()
            .filter(|agent| !args.exclude_ids.contains(&agent.id().to_string()))
            .map(|agent| {
                let mut score = 0.0;
                let mut match_reasons = Vec::new();

                let description_lower = agent.instructions.to_lowercase();
                let intent_lower = args.intent.to_lowercase();

                if description_lower.contains(&intent_lower) {
                    score += 30.0;
                    match_reasons.push("exact intent match in description".to_string());
                } else {
                    let keywords: Vec<String> = intent_lower
                        .split_whitespace()
                        .filter(|w| w.len() > 3)
                        .map(|s| s.to_string())
                        .collect();
                    let matched_keywords: Vec<String> = keywords
                        .iter()
                        .filter(|k| description_lower.contains(*k))
                        .cloned()
                        .collect();
                    if !matched_keywords.is_empty() {
                        let keyword_score =
                            (matched_keywords.len() as f64 / keywords.len() as f64) * 20.0;
                        score += keyword_score;
                        match_reasons.push(format!("keyword match: {:?}", matched_keywords));
                    }
                }

                let agent_cap_types: Vec<String> = agent
                    .manifest
                    .capabilities
                    .iter()
                    .map(|c| capability_type_name(c))
                    .collect();

                for req_cap in &args.required_capabilities {
                    if agent_cap_types.iter().any(|cap| cap == req_cap) {
                        score += 15.0;
                        match_reasons.push(format!("has required capability: {}", req_cap));
                    }
                }

                if agent
                    .manifest
                    .background
                    .as_ref()
                    .map(|b| b.enabled)
                    .unwrap_or(false)
                {
                    score += 5.0;
                    match_reasons.push("supports background execution".to_string());
                }

                let io_schema = agent.manifest.io.as_ref().map(|io| {
                    serde_json::json!({
                        "accepts": io.accepts,
                        "returns": io.returns,
                    })
                });

                AgentDiscoveryResult {
                    score,
                    agent_id: agent.id().to_string(),
                    name: agent.manifest.agent.name,
                    description: agent.manifest.agent.description,
                    capabilities: agent_cap_types,
                    match_reasons,
                    io: io_schema,
                }
            })
            .filter(|r| r.score > 0.0)
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(serde_json::json!({
            "ok": true,
            "query": {
                "intent": args.intent,
                "required_capabilities": args.required_capabilities,
            },
            "results": results,
            "result_count": results.len(),
        })
        .to_string())
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AgentSpawnTool));
    registry.register(Box::new(AgentExistsTool));
    registry.register(Box::new(AgentDiscoverTool));
}
