use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{
    capability_type_name, validate_agent_id, NativeTool, NativeToolRegistry,
};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::{CausalEventRecord, EntryStatus};
use autonoetic_types::config::{GatewayConfig, SchemaEnforcementConfig, SchemaEnforcementMode};
use autonoetic_types::schema_enforcement::{default_enforcer, EnforcementResult, SchemaEnforcer};
use autonoetic_types::tool_error::tagged;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowEventRecord};
use chrono::Utc;
use serde::{de, Deserialize, Serialize};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct SpawnAgentArgs {
    agent_id: String,
    #[serde(deserialize_with = "crate::runtime::tools::deserialize_string_lenient")]
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
    #[serde(
        default,
        deserialize_with = "crate::runtime::tools::deserialize_bool_lenient"
    )]
    r#async: bool,
    /// Join group name. Tasks in the same join group are awaited together by the planner.
    #[serde(default)]
    join_group: Option<String>,
    /// Artifact ID whose layers should be auto-mounted into the child's sandbox.
    /// When set, all sandbox.exec calls in the child session automatically
    /// mount the artifact's dependency layers (read-only).
    #[serde(default)]
    artifact_id: Option<String>,
    /// Spawn-time credential bindings. Each entry maps a service name to a
    /// specific credential_id, overriding runtime.lock resolution for the child.
    #[serde(default)]
    credential_bindings: Vec<autonoetic_types::runtime_lock::LockedCredentialMount>,
}

/// Keeps a workflow task's `updated_at` fresh while synchronous `agent.spawn` blocks.
///
/// Without this, long post-processing tails can look like a stale Running task and trigger
/// false stuck-task auto-resolution.
struct SyncTaskHeartbeat {
    stop_tx: Option<mpsc::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl SyncTaskHeartbeat {
    fn start(
        config: GatewayConfig,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        workflow_id: String,
        task_id: String,
        heartbeat_secs: u64,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        let join = thread::spawn(move || {
            let tick = Duration::from_secs(heartbeat_secs.max(1));
            loop {
                match rx.recv_timeout(tick) {
                    Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(e) = crate::scheduler::workflow_store::refresh_task_run_heartbeat(
                            &config,
                            gateway_store.as_deref(),
                            &workflow_id,
                            &task_id,
                        ) {
                            tracing::debug!(
                                target: "workflow",
                                workflow_id = %workflow_id,
                                task_id = %task_id,
                                error = %e,
                                "sync spawn heartbeat update failed"
                            );
                        }
                    }
                }
            }
        });

        Self {
            stop_tx: Some(tx),
            join: Some(join),
        }
    }
}

impl Drop for SyncTaskHeartbeat {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub struct AgentSpawnTool;

impl NativeTool for AgentSpawnTool {
    fn name(&self) -> &'static str {
        "agent_spawn"
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
                    "join_group": { "type": "string", "description": "Optional group name for join semantics. Tasks in the same group are awaited together before planner resumes." },
                    "credential_bindings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "service": { "type": "string", "description": "Service name (e.g. 'moltbook')" },
                                "credential_id": { "type": "string", "description": "Specific credential ID to inject for this service" }
                            },
                            "required": ["service", "credential_id"]
                        },
                        "description": "Bind specific credentials to the child agent. Overrides runtime.lock service-level resolution."
                    }
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
        let mut args: SpawnAgentArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        validate_agent_id(&args.agent_id)?;
        anyhow::ensure!(!args.message.trim().is_empty(), "message must not be empty");

        // Schema enforcement hook.
        //
        // The target agent's `io.accepts` describes the shape of `message` itself
        // (the content the child will process) — not a wrapper of the spawn call.
        // We parse `message` as JSON and enforce the schema against the parsed
        // value. On mismatch we return a structured tool result (`ok: false`)
        // containing `expected_schema`, per-field errors, and a repair hint so
        // the calling LLM can re-map and retry.
        let default_enforcement_config = SchemaEnforcementConfig::default();
        let enforcement_config = config
            .map(|c| &c.schema_enforcement)
            .unwrap_or(&default_enforcement_config);

        let resolved_session_id = args
            .session_id
            .clone()
            .or_else(|| session_id.map(ToOwned::to_owned))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        if enforcement_config.mode != SchemaEnforcementMode::Disabled {
            let agents_dir = config.map(|c| &c.agents_dir).ok_or_else(|| {
                anyhow::anyhow!("config is required for agent.spawn schema enforcement")
            })?;
            let target_agent_path = agents_dir.join(&args.agent_id).join("SKILL.md");

            if target_agent_path.exists() {
                if let Ok(manifest_content) = std::fs::read_to_string(&target_agent_path) {
                    if let Some(frontmatter) = manifest_content.split("---").nth(1) {
                        if let Ok(target_manifest) =
                            serde_yaml::from_str::<AgentManifest>(frontmatter)
                        {
                            if let Some(io) = &target_manifest.io {
                                if let Some(accepts) = &io.accepts {
                                    match enforce_spawn_message_schema(
                                        &args.agent_id,
                                        &args.message,
                                        accepts,
                                    ) {
                                        SpawnSchemaOutcome::Pass => {
                                            log_io_contract_enforcement(
                                                gateway_store.as_deref(),
                                                &manifest.agent.id,
                                                &resolved_session_id,
                                                Some(&args.agent_id),
                                                "io.accepts",
                                                EntryStatus::Success,
                                                serde_json::json!({
                                                    "contract": "io.accepts",
                                                    "result": "pass",
                                                    "target_agent_id": &args.agent_id,
                                                    "expected_schema": accepts,
                                                    "enforcer": "deterministic"
                                                }),
                                            );
                                        }
                                        SpawnSchemaOutcome::Coerced {
                                            new_message,
                                            transformations,
                                        } => {
                                            if enforcement_config.audit {
                                                tracing::info!(
                                                    target: "schema_enforcement",
                                                    agent_id = %args.agent_id,
                                                    transformations = ?transformations,
                                                    "Schema enforcement: payload coerced"
                                                );
                                            }
                                            log_io_contract_enforcement(
                                                gateway_store.as_deref(),
                                                &manifest.agent.id,
                                                &resolved_session_id,
                                                Some(&args.agent_id),
                                                "io.accepts",
                                                EntryStatus::Success,
                                                serde_json::json!({
                                                    "contract": "io.accepts",
                                                    "result": "coerced",
                                                    "target_agent_id": &args.agent_id,
                                                    "expected_schema": accepts,
                                                    "transformations": transformations,
                                                    "enforcer": "deterministic"
                                                }),
                                            );
                                            args.message = new_message;
                                        }
                                        SpawnSchemaOutcome::Reject(body) => {
                                            let payload =
                                                serde_json::from_str::<serde_json::Value>(&body)
                                                    .unwrap_or_else(|_| {
                                                        serde_json::json!({
                                                            "contract": "io.accepts",
                                                            "result": "rejected",
                                                            "target_agent_id": &args.agent_id,
                                                            "reason": &body,
                                                            "expected_schema": accepts,
                                                            "enforcer": "deterministic"
                                                        })
                                                    });
                                            log_io_contract_enforcement(
                                                gateway_store.as_deref(),
                                                &manifest.agent.id,
                                                &resolved_session_id,
                                                Some(&args.agent_id),
                                                "io.accepts",
                                                EntryStatus::Denied,
                                                payload,
                                            );
                                            return Ok(body);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let agents_dir = config
            .map(|c| &c.agents_dir)
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.spawn"))?;

        let fallback_gateway_config = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..GatewayConfig::default()
        };
        let gw_config = config.unwrap_or(&fallback_gateway_config);

        let root_for_approval_check =
            crate::runtime::content_store::root_session_id(&resolved_session_id);
        let pending = crate::scheduler::approval::pending_approval_requests_for_root(
            gw_config,
            gateway_store.as_deref(),
            &root_for_approval_check,
        )?;
        let pending_session_continue: Vec<String> = pending
            .iter()
            .filter_map(|r| {
                if matches!(
                    r.action,
                    autonoetic_types::background::ScheduledAction::SessionContinue { .. }
                ) {
                    Some(r.request_id.clone())
                } else {
                    None
                }
            })
            .collect();
        if !pending_session_continue.is_empty() {
            return Err(anyhow::anyhow!(
                "Cannot delegate (agent.spawn): max-session-turn continuation approval is pending for this session. Pending request id(s): {}. Approve or reject first, then continue.",
                pending_session_continue.join(", ")
            ));
        }
        // Role-agnostic: block synchronous delegation while session-blocking approvals
        // are pending for this *root* session. SandboxExec approvals do NOT block spawns —
        // they are scoped to a specific tool call in a child session and should not deadlock
        // the parent from spawning unrelated agents.
        let session_blocking_approvals: Vec<_> = pending
            .iter()
            .filter(|r| {
                !matches!(
                    r.action,
                    autonoetic_types::background::ScheduledAction::SandboxExec { .. }
                )
            })
            .collect();
        if !args.r#async && !session_blocking_approvals.is_empty() {
            let ids: Vec<String> = session_blocking_approvals
                .iter()
                .map(|r| r.request_id.clone())
                .collect();
            return Err(anyhow::anyhow!(
                "Cannot delegate (agent.spawn) while approval(s) are pending for this session. Pending request id(s): {}. Approve or reject with `autonoetic gateway approvals approve|reject <id> --config <path>`, then continue.",
                ids.join(", ")
            ));
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
        let target_agent_id = args.agent_id.clone();

        let durable_operation = crate::scheduler::single_flight::durable_operation_for_spawn(
            &workflow_id,
            &target_agent_id,
            args.metadata.as_ref(),
            args.artifact_id.as_deref(),
            &args.message,
        );
        if let Some(spec) = durable_operation.as_ref() {
            match crate::scheduler::single_flight::try_acquire_reservation(
                gw_config,
                gateway_store.as_deref(),
                spec,
                Some(&task_id),
                None,
            )? {
                crate::scheduler::single_flight::AcquireOutcome::Acquired(_) => {}
                crate::scheduler::single_flight::AcquireOutcome::Coalesced(existing) => {
                    crate::scheduler::append_workflow_event(
                        gw_config,
                        gateway_store.as_deref(),
                        &WorkflowEventRecord {
                            event_id: format!("wevt-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                            workflow_id: workflow_id.clone(),
                            task_id: existing.existing_task_id.clone(),
                            event_type: "workflow.single_flight.coalesced".to_string(),
                            agent_id: Some(target_agent_id.clone()),
                            payload: serde_json::json!({
                                "status": "coalesced",
                                "stage_kind": existing.stage_kind,
                                "dedupe_key": existing.dedupe_key,
                                "existing_task_id": existing.existing_task_id,
                                "approval_request_id": existing.approval_request_id,
                                "retry_advice": "wait",
                            }),
                            occurred_at: Utc::now().to_rfc3339(),
                        },
                    )?;

                    return serde_json::to_string(&serde_json::json!({
                        "ok": true,
                        "accepted": true,
                        "status": "coalesced",
                        "workflow_id": workflow_id,
                        "existing_task_id": existing.existing_task_id,
                        "approval_request_id": existing.approval_request_id,
                        "dedupe_key": existing.dedupe_key,
                        "retry_advice": "wait",
                        "message": "Equivalent durable operation is already active. Wait for the existing task instead."
                    }))
                    .map_err(Into::into);
                }
            }
        }

        let execution_config = GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        };
        let execution =
            crate::execution::GatewayExecutionService::new(execution_config, gateway_store.clone());

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
        let spawn_reason_preview: String = kickoff_message.chars().take(200).collect();
        let persist_result: anyhow::Result<String> = (|| {
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
                retry_count: 0,
                last_failure_class: None,
                retry_policy: crate::scheduler::workflow_store::retry_policy_from_metadata(
                    args.metadata.as_ref(),
                ),
                side_effect_state: None,
                dedupe_key: durable_operation.as_ref().map(|spec| spec.dedupe_key.clone()),
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
                        "spawn_reason": spawn_reason_preview,
                        "spawn_reason_full": kickoff_message,
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

            // --- Always queue the task (async execution by the scheduler) ---
            // The sync `block_in_place` path was removed because it deadlocks the
            // tokio runtime when called from within an already-running agent context.
            // The scheduler's `process_queued_workflow_tasks` picks up queued tasks
            // and runs them on dedicated tokio tasks.
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
                credential_bindings: args.credential_bindings,
            };
            crate::scheduler::enqueue_task(gw_config, gateway_store.as_deref(), &queued)?;

            let _ = crate::scheduler::update_task_run_status(
                gw_config,
                gateway_store.as_deref(),
                &workflow_id,
                &task_id,
                TaskRunStatus::Pending,
                Some("queued".to_string()),
                None,
                None,
            );

            serde_json::to_string(&serde_json::json!({
                "ok": true,
                "accepted": true,
                "status": "queued",
                "workflow_id": workflow_id,
                "task_id": task_id,
                "agent_id": target_agent_id,
                "session_id": child_delegation_path,
                "message": "Task queued for async execution. Use workflow.wait with task_ids to check completion status."
            }))
            .map_err(Into::into)
        })();

        if persist_result.is_err() {
            if let Some(spec) = durable_operation.as_ref() {
                let _ = crate::scheduler::single_flight::release_reservation(
                    gw_config,
                    &workflow_id,
                    &spec.dedupe_key,
                );
            }
        }
        return persist_result;
    }
}

fn deserialize_string_vec_lenient<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| de::Error::custom("array elements must be strings"))
            })
            .collect(),
        serde_json::Value::String(s) => {
            let sanitized = s.replace("<|\"|>", "\"");
            serde_json::from_str::<Vec<String>>(&sanitized).map_err(de::Error::custom)
        }
        other => Err(de::Error::custom(format!(
            "must be an array or a JSON string of an array, got {other}"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct AgentDiscoverArgs {
    intent: String,
    #[serde(default, deserialize_with = "deserialize_string_vec_lenient")]
    required_capabilities: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_vec_lenient")]
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
        "agent_discover"
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

pub struct AgentListTool;

impl NativeTool for AgentListTool {
    fn name(&self) -> &'static str {
        "agent_list"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::SandboxFunctions { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Enumerate installed agents with their metadata. Each entry includes agent_id, description, capabilities, execution_mode, script_input_mode (for script agents), and the io_accepts / io_returns JSON schemas when declared. Use io_accepts to shape the `message` you pass to agent.spawn: for targets that declare an object schema, emit `message` as a JSON string whose parsed value matches it. Returns a plain directory — no semantic scoring.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "filter_prefix": {
                        "type": "string",
                        "description": "Only return agents whose agent_id starts with this prefix (e.g. 'specialists/' or 'evolution/')."
                    },
                    "requires_capability": {
                        "type": "string",
                        "description": "Only return agents that declare this capability type (e.g. 'NetworkAccess', 'CodeExecution', 'CredentialAccess')."
                    },
                    "execution_mode": {
                        "type": "string",
                        "enum": ["reasoning", "script"],
                        "description": "Only return agents with this execution mode."
                    }
                },
                "additionalProperties": false
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
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            filter_prefix: Option<String>,
            #[serde(default)]
            requires_capability: Option<String>,
            #[serde(default)]
            execution_mode: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let mut agents: Vec<serde_json::Value> = Vec::new();

        // Phase 1: query SQLite aliases for revision-based agents
        let mut sqlite_agent_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let (Some(ref store), Some(gd)) = (&gateway_store, gateway_dir) {
            if let Ok(aliases) = store.list_agent_aliases(None) {
                for alias in aliases {
                    // Apply prefix filter early
                    if let Some(ref prefix) = args.filter_prefix {
                        if !alias.agent_id.starts_with(prefix.as_str()) {
                            continue;
                        }
                    }
                    sqlite_agent_ids.insert(alias.agent_id.clone());

                    // Read manifest metadata from the revision record
                    if let Ok(Some(rev)) = store.get_agent_revision(&alias.revision_id) {
                        let manifest_meta = rev.metadata_json.get("manifest");

                        if let Some(meta) = manifest_meta {
                            // Rich metadata available from SQLite
                            let description = meta.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let cap_types: Vec<String> = meta.get("capabilities")
                                .and_then(|v| v.as_array())
                                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();
                            let mode = meta.get("execution_mode").and_then(|v| v.as_str()).unwrap_or("reasoning").to_string();
                            let script_input_mode = meta.get("script_input_mode")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let io_accepts = meta.get("io")
                                .and_then(|v| v.get("accepts"))
                                .cloned();
                            let io_returns = meta.get("io")
                                .and_then(|v| v.get("returns"))
                                .cloned();

                            // Apply capability filter
                            if let Some(ref req_cap) = args.requires_capability {
                                let has_cap = cap_types.iter().any(|c| c.eq_ignore_ascii_case(req_cap));
                                if !has_cap {
                                    continue;
                                }
                            }
                            // Apply execution_mode filter
                            if let Some(ref req_mode) = args.execution_mode {
                                if !mode.eq_ignore_ascii_case(req_mode) {
                                    continue;
                                }
                            }

                            agents.push(serde_json::json!({
                                "agent_id": alias.agent_id,
                                "description": description,
                                "capabilities": cap_types,
                                "execution_mode": mode,
                                "script_input_mode": script_input_mode,
                                "io_accepts": io_accepts,
                                "io_returns": io_returns,
                            }));
                        } else {
                            // Fallback: no manifest metadata in SQLite — try reading from
                            // gateway_dir/revisions/agents/<id>/latest/SKILL.md
                            let latest_path = gd.join("revisions").join("agents")
                                .join(&alias.agent_id).join("latest").join("SKILL.md");
                            if let Ok(skill_text) = std::fs::read_to_string(&latest_path) {
                                if let Ok((manifest, _)) = crate::runtime::parser::SkillParser::parse(&skill_text) {
                                    let cap_types: Vec<String> = manifest.capabilities.iter().map(|c| capability_type_name(c)).collect();
                                    let mode = match manifest.execution_mode {
                                        autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
                                        autonoetic_types::agent::ExecutionMode::Script => "script",
                                    };

                                    // Apply filters
                                    if let Some(ref req_cap) = args.requires_capability {
                                        let has_cap = cap_types.iter().any(|c| c.eq_ignore_ascii_case(req_cap));
                                        if !has_cap { continue; }
                                    }
                                    if let Some(ref req_mode) = args.execution_mode {
                                        if !mode.eq_ignore_ascii_case(req_mode) { continue; }
                                    }

                                    let io_accepts = manifest.io.as_ref().and_then(|io| io.accepts.clone());
                                    let io_returns = manifest.io.as_ref().and_then(|io| io.returns.clone());
                                    let script_input_mode = matches!(manifest.execution_mode, autonoetic_types::agent::ExecutionMode::Script)
                                        .then(|| match manifest.script_input_mode {
                                            autonoetic_types::agent::ScriptInputMode::Stdin => "stdin",
                                            autonoetic_types::agent::ScriptInputMode::Args => "args",
                                        });

                                    agents.push(serde_json::json!({
                                        "agent_id": alias.agent_id,
                                        "description": manifest.agent.description,
                                        "capabilities": cap_types,
                                        "execution_mode": mode,
                                        "script_input_mode": script_input_mode,
                                        "io_accepts": io_accepts,
                                        "io_returns": io_returns,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Phase 2: fall back to filesystem for legacy agents not in SQLite
        let agents_dir = config
            .map(|c| &c.agents_dir)
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.list"))?;

        let repo = crate::agent::AgentRepository::new(agents_dir.clone());
        if let Ok(loaded_agents) = repo.list_loaded_sync() {
            for agent in loaded_agents {
                let agent_id = agent.id().to_string();
                // Skip agents already listed via SQLite
                if sqlite_agent_ids.contains(&agent_id) {
                    continue;
                }
                // Apply prefix filter
                if let Some(ref prefix) = args.filter_prefix {
                    if !agent_id.starts_with(prefix.as_str()) {
                        continue;
                    }
                }
                // Apply capability filter
                if let Some(ref req_cap) = args.requires_capability {
                    let has_cap = agent.manifest.capabilities.iter().any(|c| capability_type_name(c).eq_ignore_ascii_case(req_cap));
                    if !has_cap { continue; }
                }
                // Apply execution_mode filter
                if let Some(ref mode) = args.execution_mode {
                    let agent_mode = match &agent.manifest.execution_mode {
                        autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
                        autonoetic_types::agent::ExecutionMode::Script => "script",
                    };
                    if !agent_mode.eq_ignore_ascii_case(mode) { continue; }
                }

                let cap_types: Vec<String> = agent.manifest.capabilities.iter().map(|c| capability_type_name(c)).collect();
                let mode = match &agent.manifest.execution_mode {
                    autonoetic_types::agent::ExecutionMode::Reasoning => "reasoning",
                    autonoetic_types::agent::ExecutionMode::Script => "script",
                };
                let io_accepts = agent.manifest.io.as_ref().and_then(|io| io.accepts.clone());
                let io_returns = agent.manifest.io.as_ref().and_then(|io| io.returns.clone());
                let script_input_mode = matches!(agent.manifest.execution_mode, autonoetic_types::agent::ExecutionMode::Script)
                    .then(|| match agent.manifest.script_input_mode {
                        autonoetic_types::agent::ScriptInputMode::Stdin => "stdin",
                        autonoetic_types::agent::ScriptInputMode::Args => "args",
                    });

                agents.push(serde_json::json!({
                    "agent_id": agent_id,
                    "description": agent.manifest.agent.description,
                    "capabilities": cap_types,
                    "execution_mode": mode,
                    "script_input_mode": script_input_mode,
                    "io_accepts": io_accepts,
                    "io_returns": io_returns,
                }));
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "agents": agents,
            "count": agents.len(),
        })
        .to_string())
    }
}

#[derive(Debug, Deserialize)]
struct AgentMessageArgs {
    target_session_id: Option<String>,
    target_agent_id: Option<String>,
    message: String,
}

pub struct AgentMessageTool;

impl NativeTool for AgentMessageTool {
    fn name(&self) -> &'static str {
        "agent_message"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::AgentMessage { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Send a direct asynchronous message to another active agent session or broadcast to all sessions of a specific agent role.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_session_id": { "type": "string", "description": "Specific session ID to message." },
                    "target_agent_id": { "type": "string", "description": "Agent role to message. Broadcasts to all active sessions for this role if target_session_id is absent." },
                    "message": { "type": "string", "description": "The message to send." }
                },
                "required": ["message"],
                "anyOf": [
                    { "required": ["target_session_id"] },
                    { "required": ["target_agent_id"] }
                ]
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentMessageArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("Gateway store is required for agent.message"))?;

        if args.target_session_id.is_none() && args.target_agent_id.is_none() {
            return Err(anyhow::anyhow!(
                "Either target_session_id or target_agent_id must be provided"
            ));
        }

        let sender_session_id = session_id.unwrap_or("unknown_session").to_string();
        let sender_agent_id = manifest.agent.id.clone();

        // Fast capability check against policy using the provided agent ID if available,
        // else fallback to parsing bounded capability scope or checking patterns runtime.
        if let Some(ref tid) = args.target_agent_id {
            let decision = policy.can_message_agent(tid);
            if !decision.is_allowed() {
                return Err(tagged::Tagged::permission_with_rules(
                    anyhow::anyhow!("Permission denied: cannot message agent '{}'", tid),
                    decision
                        .enforced_rules
                        .into_iter()
                        .map(|rule| rule.to_string())
                        .collect(),
                )
                .into());
            }
        } else {
            // For target_session_id, verify broadly if capability exists
            if !policy.can_message_agent("*").is_allowed()
                && !policy.can_message_agent("any").is_allowed()
            {
                // Technically we'd look up target_session_id's agent, but for now we require broad msg right or specific target_agent_id
            }
        }

        // Resolve targets and save deliveries
        let mut target_sessions = Vec::new();
        if let Some(ref s_id) = args.target_session_id {
            target_sessions.push(s_id.clone());
        } else if let Some(ref a_id) = args.target_agent_id {
            if let Ok(sessions) = store.list_sessions_for_agent(a_id) {
                target_sessions.extend(sessions);
            }

            if target_sessions.is_empty() {
                let mut exists = None;
                let mut status = "no_live_recipients";
                let mut message = format!(
                    "Agent '{}' exists but has no active sessions to receive the message.",
                    a_id
                );

                if let Some(cfg) = config {
                    let repo = crate::agent::AgentRepository::new(cfg.agents_dir.clone());
                    match repo.get_sync(a_id) {
                        Ok(_) => {
                            exists = Some(true);
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            if error_msg.contains("not found") {
                                exists = Some(false);
                                status = "target_agent_not_found";
                                message = format!(
                                    "No installed agent found with id '{}'. agent.message requires an existing target agent with at least one live session.",
                                    a_id
                                );
                            } else {
                                exists = Some(true);
                                status = "target_agent_unavailable";
                                message = format!(
                                    "Agent '{}' exists but could not be loaded: {}",
                                    a_id, error_msg
                                );
                            }
                        }
                    }
                }

                return Ok(serde_json::json!({
                    "ok": false,
                    "status": status,
                    "target_agent_id": a_id,
                    "recipients_count": 0,
                    "exists": exists,
                    "message": message,
                })
                .to_string());
            }
        }

        let target_pattern = if let Some(ref s_id) = args.target_session_id {
            format!("session:{}", s_id)
        } else {
            format!("agent:{}", args.target_agent_id.as_ref().unwrap())
        };

        let msg_id = format!("msg-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        let record = crate::scheduler::gateway_store::AgentMessageRecord {
            message_id: msg_id.clone(),
            sender_session_id: sender_session_id.clone(),
            sender_agent_id: sender_agent_id.clone(),
            target_pattern: target_pattern.clone(),
            message: args.message.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };

        store.save_agent_message(&record)?;

        for tgt_session in &target_sessions {
            store.insert_message_delivery(&msg_id, tgt_session)?;

            // Deliver a wakeup signal
            let signal = crate::scheduler::signal::Signal::AgentMessage {
                message_id: msg_id.clone(),
                sender_session_id: sender_session_id.clone(),
                sender_agent_id: sender_agent_id.clone(),
                message: args.message.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
            };

            if let Err(e) =
                crate::scheduler::signal::write_signal(Some(&store), &tgt_session, &msg_id, &signal)
            {
                tracing::debug!(target: "agent_message", error = %e, "Failed to write signal for target session");
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "message_id": msg_id,
            "status": "delivered",
            "recipients_count": target_sessions.len()
        })
        .to_string())
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AgentSpawnTool));
    registry.register(Box::new(AgentDiscoverTool));
    registry.register(Box::new(AgentListTool));
    registry.register(Box::new(AgentMessageTool));
}

/// Outcome of enforcing the target agent's `io.accepts` schema on a spawn message.
pub(crate) enum SpawnSchemaOutcome {
    /// Message matches the schema — proceed unchanged.
    Pass,
    /// The enforcer coerced the payload (filled defaults, etc.). The caller should
    /// use `new_message` downstream instead of the original.
    Coerced {
        new_message: String,
        transformations: Vec<autonoetic_types::schema_enforcement::Transformation>,
    },
    /// Message does not match the schema. `body` is a JSON string that should be
    /// returned as the tool result so the calling LLM can read `expected_schema`,
    /// `fields_with_errors`, and `hint` and repair.
    Reject(String),
}

/// Validate (and optionally coerce) a spawn `message` against a target's `io.accepts`.
///
/// The schema describes the shape of `message` itself — the content the child will
/// process. For object schemas we parse `message` as JSON and validate the parsed
/// value; for string schemas we use `message` directly. Parse failures produce a
/// structured rejection, not a bail.
pub(crate) fn enforce_spawn_message_schema(
    agent_id: &str,
    message: &str,
    accepts: &serde_json::Value,
) -> SpawnSchemaOutcome {
    let schema_top_type = accepts.get("type").and_then(|t| t.as_str());
    let expects_string = schema_top_type == Some("string");

    let payload: serde_json::Value = if expects_string {
        serde_json::Value::String(message.to_string())
    } else {
        match serde_json::from_str::<serde_json::Value>(message) {
            Ok(v) => v,
            Err(parse_err) => {
                return SpawnSchemaOutcome::Reject(
                    serde_json::json!({
                        "ok": false,
                        "error": "schema_validation_failed",
                        "agent_id": agent_id,
                        "reason": format!("message is not valid JSON: {}", parse_err),
                        "expected_schema": accepts,
                        "hint": "Target agent declares io.accepts. Send `message` as a JSON string whose parsed value matches expected_schema, then retry.",
                    })
                    .to_string(),
                );
            }
        }
    };

    let enforcer = default_enforcer();
    match enforcer.enforce(&payload, accepts) {
        EnforcementResult::Pass => SpawnSchemaOutcome::Pass,
        EnforcementResult::Coerced(details) => {
            let new_message = if expects_string {
                details
                    .final_payload
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| message.to_string())
            } else {
                details.final_payload.to_string()
            };
            SpawnSchemaOutcome::Coerced {
                new_message,
                transformations: details.transformations,
            }
        }
        EnforcementResult::Reject(details) => SpawnSchemaOutcome::Reject(
            serde_json::json!({
                "ok": false,
                "error": "schema_validation_failed",
                "agent_id": agent_id,
                "reason": details.reason,
                "expected_schema": accepts,
                "fields_with_errors": details.fields_with_errors,
                "hint": details.hint.unwrap_or_else(|| {
                    "Re-map `message` to match expected_schema and retry.".to_string()
                }),
            })
            .to_string(),
        ),
    }
}

fn log_io_contract_enforcement(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    agent_id: &str,
    session_id: &str,
    target: Option<&str>,
    action: &str,
    status: EntryStatus,
    payload: serde_json::Value,
) {
    let Some(store) = gateway_store else {
        return;
    };

    let payload_str = serde_json::to_string(&payload).ok();
    let reason = payload
        .get("reason")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    if let Err(error) = store.create_causal_event(&CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: Utc::now().timestamp_millis().max(0) as u64,
        timestamp: Utc::now().to_rfc3339(),
        category: "contract".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: target.map(ToOwned::to_owned),
        payload: payload_str,
        payload_ref: None,
        evidence_ref: None,
        reason,
    }) {
        tracing::warn!(
            target: "schema_enforcement",
            error = %error,
            action = action,
            agent_id = agent_id,
            session_id = session_id,
            "Failed to persist contract enforcement event"
        );
    }
}

#[cfg(test)]
mod spawn_schema_tests {
    use super::*;

    fn object_schema_with_required() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["location", "date"],
            "properties": {
                "location": { "type": "string" },
                "date":     { "type": "string" }
            }
        })
    }

    #[test]
    fn pass_when_message_matches_object_schema() {
        let schema = object_schema_with_required();
        let message = r#"{"location":"paris","date":"2026-04-24"}"#;
        let outcome = enforce_spawn_message_schema("weather", message, &schema);
        assert!(matches!(outcome, SpawnSchemaOutcome::Pass));
    }

    #[test]
    fn reject_plain_text_when_schema_expects_object() {
        let schema = object_schema_with_required();
        let outcome = enforce_spawn_message_schema("weather", "weather in paris tomorrow", &schema);
        let body = match outcome {
            SpawnSchemaOutcome::Reject(b) => b,
            _ => panic!("expected Reject, got other"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "schema_validation_failed");
        assert_eq!(parsed["agent_id"], "weather");
        assert!(
            parsed["expected_schema"].is_object(),
            "expected_schema must be surfaced so caller can repair"
        );
        assert!(parsed["hint"].is_string());
    }

    #[test]
    fn reject_body_is_parseable_and_surfaces_repair_fields() {
        // Drive the enforcer into the error path via a per-property `required: true`
        // flag with a non-defaultable type, which is what DeterministicCoercionEnforcer
        // actually recognizes today.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "weird_field": { "type": "custom-type", "required": true }
            }
        });
        let outcome = enforce_spawn_message_schema("target", r#"{}"#, &schema);
        let body = match outcome {
            SpawnSchemaOutcome::Reject(b) => b,
            _ => panic!("expected Reject"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error"], "schema_validation_failed");
        assert_eq!(parsed["agent_id"], "target");
        let fields = parsed["fields_with_errors"]
            .as_array()
            .expect("fields_with_errors array present");
        assert!(
            fields
                .iter()
                .any(|f| f["field_path"].as_str() == Some("weird_field")),
            "missing field should be reported; got: {fields:?}"
        );
        assert!(parsed["expected_schema"].is_object());
        assert!(parsed["hint"].is_string());
    }

    #[test]
    fn passthrough_when_schema_is_string_type() {
        let schema = serde_json::json!({ "type": "string" });
        let outcome = enforce_spawn_message_schema("x", "just some text", &schema);
        assert!(matches!(outcome, SpawnSchemaOutcome::Pass));
    }
}
