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

pub(crate) fn target_agent_is_singleton(agents_dir: &Path, agent_id: &str) -> bool {
    let path = agents_dir.join(agent_id).join("SKILL.md");
    if !path.exists() {
        return false;
    }
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    crate::runtime::parser::SkillParser::parse(&raw)
        .map(|(m, _)| m.agent.singleton)
        .unwrap_or(false)
}

/// When the caller requests a promotion gate (`require_promotion_record`)
/// but omits `promotion_role`, derive it mechanically from the target agent
/// id. The validation gate otherwise defaults to `"evaluator"` and reports a
/// phantom `pass=false` even when the child recorded a passing verdict under
/// its own role (observed: unit_test_runner/auditor/static_evaluator tasks
/// marked Failed despite all three recording `pass=true`).
fn fill_promotion_role_if_missing(metadata: &mut serde_json::Value, target_agent_id: &str) {
    let requires_gate = metadata
        .get("require_promotion_record")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let role_present = metadata
        .get("promotion_role")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !requires_gate || role_present {
        return;
    }
    if let Some(role) = autonoetic_types::promotion::PromotionRole::for_agent_id(target_agent_id) {
        metadata["promotion_role"] = serde_json::json!(role.as_str());
    }
}

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
    /// Optional specific revision_id to execute. When provided, the child runs
    /// from that revision directory directly, bypassing alias resolution.
    /// Used for smoke-testing Candidate revisions before promotion.
    #[serde(default)]
    revision_id: Option<String>,
    /// Keep the spawned session addressable by `agent_message` after its task
    /// completes: it parks idle for this many seconds instead of terminating.
    /// Applies only when the target's manifest declares no residency of its
    /// own — a bundle-declared `agent.resident_idle_ttl_secs` always wins.
    #[serde(default)]
    resident_idle_ttl_secs: Option<u64>,
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

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        // The Ri-0.14 yield/join doctrine, uniform across every spawner (#466).
        // The heavy orchestrators (planner, agent-factory) keep their detailed,
        // role-specific coordination sections; this guarantees the light spawners
        // (agent-adapter, evolution-steward, specialized_builder) get the rule too.
        vec![GuidanceBlock {
            id: "orchestration.coordinate_children",
            when: GuidanceCondition::ToolPresent("agent_spawn"),
            priority: 7,
            prose: "**Coordinating children — yield, don't block or poll.** Delegating means \
**actually calling `agent_spawn`**: emitting a `delegated` status *without* a real \
`agent_spawn` call in this turn does nothing — the workflow just ends and no child runs. So to \
delegate: call `agent_spawn` with `async=true` **first**, then reply with a short status and end \
your turn — the gateway suspends you as `WaitingForChild` and wakes you automatically when the child \
reaches a terminal state or a gate (Ri-0.14), with its typed state in your turn-start context. For a \
parallel fan-out you must fully join: call `workflow_wait(task_ids=[…])` **once** (a join, not a \
poll). **Never** loop `workflow_wait` or spin `workflow_state` to discover progress — the wake-up or \
the single join already does that."
                .to_string(),
        }]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Delegate a task to a specialist agent. With async=false (default), blocks until the child completes and returns its reply. With async=true, returns immediately with a task_id — use workflow_wait to check status. Spawn multiple children in parallel with async=true, then wait for all of them. The `message` is free-form natural language for reasoning agents (researcher/architect/coder/etc. — the common case); you do NOT need to look up an input schema before spawning them. Only when the target declares an object `io.accepts` schema (agent.list reports `message_format: \"json_schema\"`) must `message` be a JSON string matching it.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "message": { "type": "string", "description": "The task to delegate, as free-form natural language. Should be self-contained; avoid dumping full conversation history. Pass a JSON string only if the target declares an object io.accepts schema (message_format: json_schema)." },
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
                    },
                    "revision_id": { "type": "string", "description": "Optional specific revision_id to execute. Bypasses alias resolution and runs the candidate revision directly. Used for smoke-testing before promotion." },
                    "resident_idle_ttl_secs": { "type": "integer", "description": "Keep the spawned session addressable by agent_message after its task completes: it parks idle for this many seconds instead of terminating. Set this when you plan to message the child after spawn returns — otherwise a completed child is unreachable. Applies only when the target bundle declares no residency of its own; clamped to 3600." }
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
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
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
            // Candidate-revision spawns (smoke tests) run a revision that is not
            // installed yet — the live `agents/<id>/SKILL.md` either doesn't exist
            // or describes a different (older) revision. Resolve the manifest from
            // the pinned revision dir so input validation keys off the exact
            // contract the child will execute against.
            let target_agent_path = spawn_target_skill_path(
                agents_dir,
                &crate::execution::gateway_root_dir(config.expect("checked above")),
                &args.agent_id,
                args.revision_id.as_deref(),
            )
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid revision_id '{}': must be a single path component (no separators or traversal)",
                    args.revision_id.as_deref().unwrap_or("")
                )
            })?;

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

        // `config` is proven `Some` by the `ok_or_else` above. The old code
        // built a fallback `GatewayConfig` here anyway, deriving its paths from
        // `agents_dir` — unreachable, and unspellable now that the gateway dir
        // is a configured field rather than a derivable suffix.
        let gw_config = config.expect("config presence checked above");

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

        // Phase 0 completion guard: do not enqueue new work once the workflow is terminal.
        // The root planner session is allowed to reactivate a *Completed* workflow so it
        // can perform follow-up work (e.g. installing and invoking a newly built agent).
        // Reactivation persists the workflow as Resumable; normal workflow spawn rules
        // then apply, and try_complete_workflow will re-close it when the root session ends.
        // NOTE: This check is not atomic with the subsequent save_task_run/enqueue_task.
        // A narrow TOCTOU window exists if the workflow transitions to terminal between
        // this check and task enqueue. The impact is bounded: the enqueued task will be
        // orphaned but the notification pump suppresses its completion signal for the
        // terminal workflow, so it cannot wake the root session. A fully atomic
        // check-and-enqueue inside a workflow-store transaction is tracked as a
        // follow-up (see RFC §4.3).
        let mut run = crate::scheduler::workflow_store::load_workflow_run(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )?.ok_or_else(|| {
            anyhow::anyhow!("Workflow '{}' vanished after terminal check", workflow_id)
        })?;
        match run.status {
            autonoetic_types::workflow::WorkflowRunStatus::Completed
                if resolved_session_id == root =>
            {
                tracing::info!(
                    target: "agent_spawn",
                    workflow_id = %workflow_id,
                    root_session_id = %root,
                    agent_id = %args.agent_id,
                    "Reactivating completed workflow for root-planner spawn"
                );
                run.status = autonoetic_types::workflow::WorkflowRunStatus::Resumable;
                run.reactivated_for_root_spawn = true;
                run.updated_at = Utc::now().to_rfc3339();
                crate::scheduler::workflow_store::save_workflow_run(
                    gw_config,
                    gateway_store.as_deref(),
                    &run,
                )?;
            }
            autonoetic_types::workflow::WorkflowRunStatus::Completed
            | autonoetic_types::workflow::WorkflowRunStatus::Failed
            | autonoetic_types::workflow::WorkflowRunStatus::Cancelled
            | autonoetic_types::workflow::WorkflowRunStatus::EmergencyStopped => {
                return Err(anyhow::anyhow!(
                    "Cannot delegate (agent.spawn): workflow {} is already terminal ({}). No new tasks can be spawned.",
                    workflow_id,
                    run.status.as_str()
                ));
            }
            _ => {}
        }

        if let Some(gate) = crate::scheduler::workflow_approval_gate_active(
            gw_config,
            gateway_store.as_deref(),
            &workflow_id,
        )? {
            let approval_ids = if gate.pending_approval_request_ids.is_empty() {
                "see awaiting task(s)".to_string()
            } else {
                gate.pending_approval_request_ids.join(", ")
            };
            return Err(anyhow::anyhow!(
                "Cannot delegate (agent.spawn): workflow {} has task(s) awaiting operator approval. Awaiting task id(s): {}. Pending approval request id(s): {}. Resolve with `autonoetic gateway approvals approve|reject <id> --config <path>` before spawning new tasks.",
                gate.workflow_id,
                gate.awaiting_task_ids.join(", "),
                approval_ids,
            ));
        }

        // Mechanical guard: block install spawns while federation gate tasks
        // are still Running. Prevents the planner from racing ahead to install
        // before unit_test_runner / sealed_evaluator / auditor finish.
        let is_install_agent = args.agent_id.contains("agent-factory")
            || args.agent_id.contains("specialized_builder");
        if is_install_agent {
            if let Some(gs) = gateway_store.as_ref() {
                if let Ok(tasks) = crate::scheduler::workflow_store::list_task_runs_for_workflow(
                    gw_config,
                    Some(gs),
                    &workflow_id,
                ) {
                    let federation_agents = [
                        "unit_test_runner",
                        "sealed_evaluator",
                        "static_evaluator",
                        "auditor",
                    ];
                    let active_federation: Vec<&str> = tasks
                        .iter()
                        .filter(|t| {
                            use autonoetic_types::workflow::TaskRunStatus as TRS;
                            matches!(
                                t.status,
                                TRS::Running | TRS::Pending | TRS::Runnable
                            ) && federation_agents.iter().any(|fa| t.agent_id.contains(fa))
                        })
                        .map(|t| t.agent_id.as_str())
                        .collect();
                    if !active_federation.is_empty() {
                        return Err(anyhow::anyhow!(
                            "Cannot spawn '{}' while federation gate tasks are still running: [{}]. Wait for them to complete (workflow_wait) before starting install.",
                            args.agent_id,
                            active_federation.join(", ")
                        ));
                    }
                }
            }
        }

        let task_id = crate::scheduler::new_task_id();
        let target_agent_id = args.agent_id.clone();
        // Resolve the target's declared egress output floor once (#971): it
        // travels in every spawn result so the session room can mark the spawn
        // row with the bundle's own output restriction. None ⇒ the bundle is
        // unrestricted or its manifest couldn't be read.
        // A missing gateway_dir means the revision store is unreachable, so a
        // revision-pinned spawn has no manifest to read. The floor is advisory,
        // so degrade to "unrestricted" rather than deriving a path.
        let target_output_label = gateway_dir.and_then(|gw| {
            resolve_target_output_label(
                agents_dir,
                gw,
                &target_agent_id,
                args.revision_id.as_deref(),
            )
        });

        // Spawn-time stale-wrapper advisory (#1221, proposal Phase 3): when the
        // target is a wrapper whose base moved on since generation, tell the
        // caller in the result. Reads the promoted revision's stored manifest
        // summary — no disk walk — and under-claims (no note) when the summary
        // predates provenance or cannot be parsed.
        let stale_wrapper_note = gateway_store.as_deref().and_then(|store| {
            let target_rev = if let Some(pinned) = args.revision_id.as_deref() {
                store.get_agent_revision(pinned).ok().flatten()
            } else {
                store
                    .resolve_alias(&target_agent_id)
                    .ok()
                    .flatten()
                    .and_then(|alias| store.get_agent_revision(&alias.revision_id).ok().flatten())
            };
            let adapter = target_rev.and_then(|rev| {
                rev.metadata_json
                    .get("manifest")
                    .and_then(|m| m.get("adapter"))
                    .and_then(|a| {
                        serde_json::from_value::<autonoetic_types::agent::AdapterProvenance>(
                            a.clone(),
                        )
                        .ok()
                    })
            });
            crate::runtime::tools::stale_wrapper_note(store, &target_agent_id, adapter.as_ref())
        });

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
                            event_id: autonoetic_types::id_format::short_random_id("wevt-"),
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

                    let mut coalesced_resp = serde_json::json!({
                        "ok": true,
                        "accepted": true,
                        "status": "coalesced",
                        "target_egress_output_label": target_output_label.map(|n| n.as_str()),
                        "workflow_id": workflow_id,
                        "existing_task_id": existing.existing_task_id,
                        "approval_request_id": existing.approval_request_id,
                        "dedupe_key": existing.dedupe_key,
                        "retry_advice": "wait",
                        "message": "Equivalent durable operation is already active. Wait for the existing task instead."
                    });
                    if let Some(ref note) = stale_wrapper_note {
                        coalesced_resp["gateway_note"] = serde_json::json!(note);
                    }
                    return serde_json::to_string(&coalesced_resp).map_err(Into::into);
                }
            }
        }

        let is_singleton = target_agent_is_singleton(agents_dir, &target_agent_id);
        let mut acquired_singleton_slot = false;
        let mut existing_singleton_task_id: Option<String> = None;
        if is_singleton {
            if let Some(gs) = gateway_store.as_ref() {
                match gs.acquire_singleton_slot(
                    &workflow_id,
                    &target_agent_id,
                    args.revision_id.as_deref(),
                    &task_id,
                ) {
                    Ok(Some(existing)) => {
                        existing_singleton_task_id = Some(existing);
                    }
                    Ok(None) => {
                        acquired_singleton_slot = true;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "singleton_dedup",
                            workflow_id = %workflow_id,
                            agent_id = %target_agent_id,
                            error = %e,
                            "failed to acquire singleton slot; proceeding without dedup"
                        );
                    }
                }
            }
        }

        if let Some(existing_task_id) = existing_singleton_task_id {
            crate::scheduler::append_workflow_event(
                gw_config,
                gateway_store.as_deref(),
                &WorkflowEventRecord {
                    event_id: autonoetic_types::id_format::short_random_id("wevt-"),
                    workflow_id: workflow_id.clone(),
                    task_id: Some(existing_task_id.clone()),
                    event_type: "workflow.singleton.deduplicated".to_string(),
                    agent_id: Some(target_agent_id.clone()),
                    payload: serde_json::json!({
                        "status": "deduplicated",
                        "requested_task_id": task_id,
                        "existing_task_id": existing_task_id,
                        "agent_id": target_agent_id,
                        "revision_id": args.revision_id,
                    }),
                    occurred_at: Utc::now().to_rfc3339(),
                },
            )?;

            let mut dedup_resp = serde_json::json!({
                "ok": true,
                "accepted": true,
                "status": "deduplicated",
                "target_egress_output_label": target_output_label.map(|n| n.as_str()),
                "singleton": true,
                "deduplicated": true,
                "workflow_id": workflow_id,
                "task_id": existing_task_id,
                "agent_id": target_agent_id,
                "message": "Singleton agent already has an active task in this workflow. Returning the existing task."
            });
            if let Some(ref note) = stale_wrapper_note {
                dedup_resp["gateway_note"] = serde_json::json!(note);
            }
            return serde_json::to_string(&dedup_resp).map_err(Into::into);
        }

        // Run on the operator's actual config. This used to fabricate one from
        // `agents_dir`, which silently gave the spawned execution a different
        // gateway dir (and so a different store, vault and revision root) than
        // the engine that created it.
        let execution = crate::execution::GatewayExecutionService::new(
            gw_config.clone(),
            gateway_store.clone(),
        );

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
            autonoetic_types::id_format::short_random_id("")
        );

        let spawned_at_turn = turn_id.and_then(crate::runtime::checkpoint::turn_number_from_id);

        let ts = Utc::now().to_rfc3339();
        let spawn_reason_preview: String = kickoff_message.chars().take(200).collect();
        let persist_result: anyhow::Result<String> = (|| {
            // Build a single metadata value that is persisted on the TaskRun and
            // forwarded to the queued task. This ensures the smoke-test gate can
            // verify the task after it completes, even when the caller supplied
            // no metadata or non-object metadata.
            let mut spawn_metadata = match args.metadata.clone() {
                Some(serde_json::Value::Object(obj)) => serde_json::Value::Object(obj),
                Some(other) => {
                    let mut obj = serde_json::Map::new();
                    obj.insert("_original_metadata".to_string(), other);
                    serde_json::Value::Object(obj)
                }
                None => serde_json::Value::Object(serde_json::Map::new()),
            };
            // `args.metadata` is model-authored and is forwarded into the child's
            // ingest, where the reserved label keys are read as *declarations*.
            // Strip them before the gateway stamps its own: an agent must not be
            // able to forge an operator mark, a peer wire label, or a parent
            // taint (I-14). Stripping rather than merely overwriting matters
            // because the gateway writes nothing when the parent is clean —
            // precisely the case a forged key would survive.
            //
            // Intersection means a forged key could only ever over-restrict, not
            // leak; the reason to close it is that label resolution must not be a
            // function of model output at all.
            let smuggled =
                crate::runtime::egress_labeler::strip_ingest_label_keys(&mut spawn_metadata);
            if !smuggled.is_empty() {
                tracing::warn!(
                    target: "egress",
                    session_id = %resolved_session_id,
                    agent_id = %source_agent_id,
                    keys = ?smuggled,
                    "agent_spawn metadata carried reserved egress label keys — stripped \
                     (agents do not author labels, I-14)"
                );
            }
            if let Some(ref rev_id) = args.revision_id {
                spawn_metadata["_autonoetic_spawn_revision_id"] = serde_json::json!(rev_id);
            }
            if let Some(ttl) = args.resident_idle_ttl_secs {
                spawn_metadata[crate::execution::SPAWN_RESIDENT_TTL_METADATA_KEY] =
                    serde_json::json!(ttl);
            }
            fill_promotion_role_if_missing(&mut spawn_metadata, &target_agent_id);
            // Persist the RAW spawn message (before [Context]/[Metadata] framing
            // is added for the child) so the smoke-test gate can compare the
            // operator's `smoke_test_input` against what was actually sent,
            // independent of the presentation-layer wrapping (issue #648).
            spawn_metadata["_autonoetic_spawn_message"] = serde_json::json!(args.message);

            // Downward taint propagation (RFC §5.5, #982). The delegation
            // instruction is derived from the parent's context, so a child
            // spawned out of a tainted parent must not start clean: it would
            // receive the private content as an *unlabeled* first user turn and
            // ship it to whatever provider its own routing picks.
            //
            // Federation inbound already seeds a receiving session's taint; a
            // local spawn is the strictly easier case and was the one left open.
            // Stamped from the parent's accumulated taint, not from anything the
            // delegating model said (I-14) — the agent chooses whether to spawn,
            // never what label rides along. The recipient side is the ingest
            // resolver, which intersects this with the child's session policy and
            // both seeds the child's taint and labels its first turn.
            //
            // Mirrors how `ecosystem.send_message` stamps a sibling payload —
            // same accessor, same reason, other direction.
            if let Some(parent_taint) = crate::runtime::egress_labeler::resolve_session_egress_taint(
                run_context,
                gateway_store.as_deref(),
                Some(resolved_session_id.as_str()),
            )
            .unwrap_or_else(|e| {
                // A failed read must not silently mean "clean child". Fail closed
                // to local_only: over-tainting a child is recoverable through
                // declassification, an unlabeled leak is not (§2.2).
                tracing::warn!(
                    target: "egress",
                    error = %e,
                    session_id = %resolved_session_id,
                    "parent taint read failed at spawn — failing closed to local_only \
                     for the child"
                );
                Some(autonoetic_types::egress::EgressLabel::local_only())
            })
            .filter(|t| !t.is_unrestricted())
            {
                spawn_metadata[crate::runtime::egress_labeler::PARENT_TAINT_METADATA_KEY] =
                    serde_json::to_value(&parent_taint).unwrap_or(serde_json::Value::Null);
            }

            let task = TaskRun {
                task_id: task_id.clone(),
                workflow_id: workflow_id.clone(),
                agent_id: target_agent_id.clone(),
                session_id: child_delegation_path.clone(),
                parent_session_id: resolved_session_id.clone(),
                // Created `Pending`: the task is queued, not yet executing.
                // `process_queued_workflow_tasks` flips it to `Running` when it
                // acquires the claim. (Previously the row was created `Running`
                // and a `Running → Pending` update was refused by the
                // transition guard — an "illegal transition" warning on every
                // spawn, and a queued-not-started task that
                // `workflow.cancel_task` could not cancel.)
                status: TaskRunStatus::Pending,
                created_at: ts.clone(),
                updated_at: ts.clone(),
                source_agent_id: Some(source_agent_id.clone()),
                result_summary: None,
                join_group: None,
                message: Some(kickoff_message.clone()),
                metadata: Some(spawn_metadata.clone()),
                retry_count: 0,
                last_failure_class: None,
                retry_policy: crate::scheduler::workflow_store::retry_policy_from_metadata(
                    args.metadata.as_ref(),
                )
                .or_else(crate::scheduler::workflow_store::default_child_retry_policy),
                side_effect_state: None,
                dedupe_key: durable_operation.as_ref().map(|spec| spec.dedupe_key.clone()),
            };
            crate::scheduler::save_task_run(gw_config, gateway_store.as_deref(), &task)?;
            crate::scheduler::append_workflow_event(
                gw_config,
                gateway_store.as_deref(),
                &WorkflowEventRecord {
                    event_id: autonoetic_types::id_format::short_random_id("wevt-"),
                    workflow_id: workflow_id.clone(),
                    task_id: Some(task_id.clone()),
                    event_type: "task.spawned".to_string(),
                    agent_id: Some(target_agent_id.clone()),
                    payload: serde_json::json!({
                        "target_agent_id": target_agent_id,
                        "child_session_id": child_delegation_path,
                        "parent_session_id": resolved_session_id,
                        "spawned_at_turn": spawned_at_turn,
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
                    "spawned_at_turn": spawned_at_turn,
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

            if let (Some(gs), Some(turn)) = (gateway_store.as_deref(), spawned_at_turn) {
                if let Err(e) = gs.upsert_session_spawn_lineage(
                    &child_delegation_path,
                    &resolved_session_id,
                    root,
                    turn,
                    &target_agent_id,
                    &ts,
                ) {
                    tracing::warn!(
                        target: "session_spawn_lineage",
                        error = %e,
                        child_session_id = %child_delegation_path,
                        spawned_at_turn = turn,
                        "Failed to record spawn lineage for child session"
                    );
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
                metadata: Some(spawn_metadata),
                join_group: args.join_group,
                blocks_planner: true,
                enqueued_at: Utc::now().to_rfc3339(),
                credential_bindings: args.credential_bindings,
            };
            crate::scheduler::enqueue_task(gw_config, gateway_store.as_deref(), &queued)?;

            let mut resp = serde_json::json!({
                "ok": true,
                "accepted": true,
                "status": "queued",
                "target_egress_output_label": target_output_label.map(|n| n.as_str()),
                "workflow_id": workflow_id,
                "task_id": task_id,
                "agent_id": target_agent_id,
                "session_id": child_delegation_path,
                "message": "Task queued for async execution. Use workflow_wait with task_ids to check completion status."
            });
            if let Some(rev_id) = args.revision_id.as_deref() {
                resp["revision_id"] = serde_json::json!(rev_id);
                resp["smoke_test"] = serde_json::json!(true);
            }
            if let Some(ref note) = stale_wrapper_note {
                // Truncation-exempt key — the advisory survives result truncation.
                resp["gateway_note"] = serde_json::json!(note);
            }
            serde_json::to_string(&resp)
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
            if acquired_singleton_slot {
                if let Some(gs) = gateway_store.as_ref() {
                    let _ = gs.release_singleton_slot_by_task_id(&workflow_id, &task_id);
                }
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
    /// Positive hint for shaping the `agent.spawn` `message`: `"free_text"`
    /// (reasoning agents — spawn directly) or `"json_schema"` (pass JSON
    /// matching `io.accepts`). Mirrors the `agent.list` field so a discovered
    /// candidate can be spawned without a second roster lookup.
    message_format: &'static str,
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
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentDiscoverArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.intent.trim().is_empty(), "intent must not be empty");

        let config = config
            .ok_or_else(|| anyhow::anyhow!("config is required for agent.discover"))?;
        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("gateway store is required for agent.discover"))?;

        // Discovery advertises agents to a spawner that will delegate to them,
        // so it must describe what would actually run: the promoted revision,
        // not the ungated `agents_dir` copy (#1136). Otherwise a delegate can
        // be chosen on capabilities its executed revision does not have.
        // Prefer the gateway_dir the engine passed in; re-deriving it from
        // config can disagree with where the store actually lives.
        let repo = crate::agent::AgentRepository::new(config.agents_dir.clone());
        let gw_dir = gateway_dir
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| crate::execution::gateway_root_dir(config));
        let loaded_agents = repo.list_loaded_from_store(&gw_dir, store.as_ref())?;

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

                let accepts = agent.manifest.io.as_ref().and_then(|io| io.accepts.clone());
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
                    message_format: crate::runtime::tools::message_format_hint(accepts.as_ref()),
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
            description: "Enumerate installed agents with their metadata. Each entry includes agent_id, description, capabilities, execution_mode, script_input_mode (for script agents), io_accepts / io_returns JSON schemas when declared, a `message_format` hint, and — for wrapper agents derived from a base agent — `adapter` composition provenance with a computed `stale_base` flag (`true`: the base was re-promoted since the wrapper was generated or is gone — regenerate via agent-adapter.default; `null`: unknown — the provenance claims no digest or the base could not be resolved; never treat `null` as current). Use `message_format` to shape the `message` you pass to agent.spawn: `\"free_text\"` means the target is a reasoning agent that takes a plain natural-language task — spawn it directly, no schema needed (`io_accepts` is null for these and that is expected, not missing data); `\"json_schema\"` means emit `message` as a JSON string whose parsed value matches `io_accepts`. Returns a plain directory — no semantic scoring. This is a read-only directory: one call gives you everything; calling it repeatedly will not surface new fields. If you already know the agent_id (e.g. a foundational specialist or a plan step's agent_id), skip this and spawn directly.".to_string(),
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
        // Guardrail: block agent_list while a post-approval wake hint is
        // active. The planner must use agent.spawn with the explicit agent_id
        // from the wake message — calling agent_list here would waste a turn
        // and risk LoopGuard degradation.
        if let Some(ctx) = _run_context {
            if let Some(ref hint) = ctx.wake_hint {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error": "post_approval_wake_active",
                    "message": format!(
                        "A plan approval wake is active for step '{}' (plan '{}', v{}). \
                         Use agent.spawn with agent_id='{}' — do NOT call agent_list.",
                        hint.step_id, hint.plan_id, hint.plan_version, hint.agent_id
                    ),
                    "hint": {
                        "plan_id": hint.plan_id,
                        "plan_version": hint.plan_version,
                        "agent_id": hint.agent_id,
                        "step_id": hint.step_id,
                    }
                }).to_string());
            }
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let mut agents: Vec<serde_json::Value> = Vec::new();

        // Enumerate promoted aliases — the complete set of agents a caller can
        // actually spawn or delegate to (#1136).
        //
        // Derive the gateway dir from config when the engine passed none. This
        // used to be gated on `gateway_dir` being present, which was harmless
        // while a filesystem phase followed it; with that phase gone, a missing
        // `gateway_dir` would silently return an empty listing.
        let resolved_gateway_dir = gateway_dir
            .map(|p| p.to_path_buf())
            .or_else(|| config.map(crate::execution::gateway_root_dir));
        if let (Some(ref store), Some(gd)) = (&gateway_store, resolved_gateway_dir.as_deref()) {
            if let Ok(aliases) = store.list_agent_aliases(None) {
                for alias in aliases {
                    // Apply prefix filter early
                    if let Some(ref prefix) = args.filter_prefix {
                        if !alias.agent_id.starts_with(prefix.as_str()) {
                            continue;
                        }
                    }

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

                            // Composition provenance (#1202): wrappers carry an
                            // `adapter` block; `stale_base` compares the digest
                            // claimed at generation against the base's currently
                            // promoted revision. Pre-#1204 summary rows have no
                            // `adapter` key — null, not an error.
                            let adapter = meta.get("adapter").cloned();
                            let adapter_prov: Option<autonoetic_types::agent::AdapterProvenance> =
                                adapter
                                    .as_ref()
                                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                            let stale_base = crate::runtime::tools::adapter_base_stale(
                                store.as_ref(),
                                adapter_prov.as_ref(),
                            );

                            agents.push(serde_json::json!({
                                "agent_id": alias.agent_id,
                                "description": description,
                                "capabilities": cap_types,
                                "execution_mode": mode,
                                "script_input_mode": script_input_mode,
                                "io_accepts": io_accepts,
                                "io_returns": io_returns,
                                "message_format": crate::runtime::tools::message_format_hint(io_accepts.as_ref()),
                                "adapter": adapter,
                                "stale_base": stale_base,
                            }));
                        } else {
                            // Fallback: no manifest metadata in SQLite — read the
                            // SKILL.md of the revision this alias actually points at.
                            //
                            // This used to read `<id>/latest/SKILL.md`. `latest` is a
                            // separate, best-effort symlink (`update_latest_symlink`
                            // ignores its own errors) maintained by different code paths
                            // than the alias, so it can point at a revision the alias
                            // does not — reintroducing the advertised-vs-executed
                            // divergence this change exists to close (#1136). The alias
                            // is the authoritative pointer, so read through it.
                            let rev_path = crate::agent::agent_revision_dir(
                                gd,
                                &alias.agent_id,
                                &alias.revision_id,
                            )
                            .join("SKILL.md");
                            if let Ok(skill_text) = std::fs::read_to_string(&rev_path) {
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
                                        "message_format": crate::runtime::tools::message_format_hint(io_accepts.as_ref()),
                                        "adapter": serde_json::to_value(&manifest.adapter).unwrap_or(serde_json::Value::Null),
                                        "stale_base": crate::runtime::tools::adapter_base_stale(
                                            store.as_ref(),
                                            manifest.adapter.as_ref(),
                                        ),
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        // There used to be a second phase here that scanned `agents_dir` for
        // "legacy agents not in SQLite" and appended them to the listing.
        // Removed in #1136: by construction those are exactly the agents with
        // *no* promoted revision — ungated on-disk manifests, or bundles
        // mid-ingest. Listing them told a spawner it could delegate to an agent
        // that has never passed a promotion gate, and advertised capabilities
        // from a file that does not govern any run. Reference bundles under
        // `agents/**` are auto-promoted at startup by `bootstrap_agents`, so a
        // legitimately installed agent always has an alias and is always
        // enumerated above.

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

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        vec![
            GuidanceBlock {
                id: "agent_message.send",
                when: GuidanceCondition::Capability("agent_message"),
                priority: 7,
                prose: "**Inter-agent messaging (`agent_message`):** Use `agent_message` for \
asynchronous fire-and-forget communication — progress updates, findings, divergence reports, or \
notifications that don't need a synchronous reply. Unlike `agent_spawn`, this does NOT create a \
child session or expect a completed-task wake-up; the receiver finds the message as a \
`[Direct Message from Agent '<sender>' (Session: <sender_session>)]` user-text block at the \
start of their next turn.\n\n\
Choose `target_agent_id` to broadcast to every unfinished session of that role (your own session \
is never included), or `target_session_id` to reach one specific session. After sending, validate \
the result: success only when `ok == true`, `status == \"delivered\"`, and `recipients_count > 0`. \
Failure statuses, none of which sent anything: `no_live_recipients` (the role is installed but has \
no unfinished session other than yours), `target_session_finished` (that session has already \
ended — it will not wake again, so nothing was queued), `target_agent_not_found` (no agent with \
that id is installed), `target_agent_unavailable` (the agent is installed but its manifest could \
not be loaded — a broken bundle, not a missing one), `target_session_not_found` (that session id \
has no agent binding, so the gateway cannot tell who owns it), \
`recipient_refuses_peer_messages` (that agent's manifest declines peer mail from you — see \
below), `recipient_consent_unverifiable` (its manifest could not be read, so consent could not \
be checked and nothing was queued).\n\n\
A child session you spawned is usually finished by the time you would message it. If you plan to \
message the child after spawn returns, pass `resident_idle_ttl_secs` to `agent_spawn` so it parks \
idle and stays addressable instead of terminating. A message that reaches a child during its final \
turn still lands: the session parks briefly at close to drain it. Otherwise `agent_message` only \
reaches a session that is still running or parked.\n\n\
Your `AgentMessage` capability `patterns` are enforced on the receiving agent's id in both \
addressing modes — with `target_session_id` the gateway resolves the session's bound agent and \
checks that. A session id does not widen your grant. A pattern ending in `*` is a prefix \
(`watchdog.*`); a pattern without one names exactly that agent.\n\n\
Your grant is only half the check. The receiving agent's manifest declares who may write to it \
(`messaging.accepts_from`), and a role whose verdict gates your work — evaluators, the security \
sentinel, the auditor, the ombudsman — refuses peer mail outright. That is a boundary, not a \
bug to route around: raise the point through the workflow or the operator, and do not retry the \
send or look for another id that reaches the same office."
                    .to_string(),
            },
            GuidanceBlock {
                id: "agent_message.receive",
                when: GuidanceCondition::Capability("agent_message"),
                priority: 6,
                prose: "**Receiving agent messages:** Messages from other agents arrive at the \
start of your turn as a user-text block: \
`[Direct Message from Agent '<sender>' (Session: <sender_session>)]` followed by the message \
content on a new line. That block is the message. A preceding \
`[Gateway] Wake-up: direct message ...` line is only the notice that woke you and never repeats \
the content — read the block, not the notice.\n\n\
Treat these as asynchronous input from a peer agent. Process the content and correlate it with \
your active goals or workflow state. If a response is needed, use `agent_message` back to the \
sender's `agent_id` or `session_id`. Do not ignore or discard incoming messages — they carry \
important signals (progress reports, divergence findings, status updates from spawned agents)."
                    .to_string(),
            },
        ]
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Send a direct asynchronous message to another unfinished agent session, or broadcast to every unfinished session of an agent role. At least one of target_session_id or target_agent_id must be provided; the gateway validates this at execution time. Your AgentMessage patterns are enforced on the receiving agent in both modes. Check `ok`, `status`, and `recipients_count` — only `recipients_count > 0` means it was queued for someone.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_session_id": { "type": "string", "description": "Specific session ID to message. Takes precedence over target_agent_id, and is the target the capability check is applied to (via that session's bound agent)." },
                    "target_agent_id": { "type": "string", "description": "Agent role to message. Broadcasts to every unfinished session of this role, excluding your own, when target_session_id is absent." },
                    "message": { "type": "string", "description": "The message to send." }
                },
                "required": ["message"]
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
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AgentMessageArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("Gateway store is required for agent_message"))?;

        if args.target_session_id.is_none() && args.target_agent_id.is_none() {
            return Err(anyhow::anyhow!(
                "Either target_session_id or target_agent_id must be provided"
            ));
        }

        let sender_session_id = session_id.unwrap_or("unknown_session").to_string();
        let sender_agent_id = manifest.agent.id.clone();

        // Capability check (P-11.5) against the agent that will actually
        // receive the message.
        //
        // `target_session_id` takes precedence for delivery below, so it must
        // also be what the ACL is evaluated against — resolved to its bound
        // agent id. Checking `target_agent_id` while delivering to
        // `target_session_id` would let a narrowly-scoped grant
        // (`patterns: ["watchdog.*"]`) reach any session by id simply by
        // naming an allowed role alongside an arbitrary session.
        let acl_target_agent_id = match (&args.target_session_id, &args.target_agent_id) {
            (Some(sid), _) => match store.get_session_agent_binding(sid)? {
                Some(binding) => {
                    // A session that cannot consume a delivery must not be
                    // queued for: the liveness filter applies to both
                    // addressing modes, not just the one that enumerates
                    // sessions itself.
                    //
                    // "Has an outcome row" is NOT that test (#1231). A room or
                    // root session writes one at every close and wakes again on
                    // the next operator turn, so presence-as-terminality made
                    // the most valuable recipient — the orchestrating parent —
                    // permanently unreachable while it was still running.
                    // `is_session_addressable` asks the ordering question
                    // instead: has it woken since it last closed?
                    if !store.is_session_addressable(sid)? {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "status": "target_session_finished",
                            "target_session_id": sid,
                            "target_agent_id": binding.agent_id,
                            "recipients_count": 0,
                            "message": format!(
                                "Session '{}' (agent '{}') has closed and has not woken since, so it \
                                 cannot receive a message — deliveries are injected by a running \
                                 session. Nothing was queued. Use agent_spawn if the work still \
                                 needs doing.",
                                sid, binding.agent_id
                            ),
                        })
                        .to_string());
                    }
                    binding.agent_id
                }
                None => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "status": "target_session_not_found",
                        "target_session_id": sid,
                        "recipients_count": 0,
                        "message": format!(
                            "No agent binding found for session '{}'. agent_message cannot verify \
                             which agent owns an unknown session, so it will not deliver to it. \
                             Pass target_agent_id to reach a role, or a session id from this workflow.",
                            sid
                        ),
                    })
                    .to_string());
                }
            },
            (None, Some(tid)) => tid.clone(),
            // Guarded above: at least one target is always present here.
            (None, None) => unreachable!("target presence validated above"),
        };

        let decision = policy.can_message_agent(&acl_target_agent_id);
        if !decision.is_allowed() {
            return Err(tagged::Tagged::permission_with_rules(
                anyhow::anyhow!(
                    "Permission denied: cannot message agent '{}'",
                    acl_target_agent_id
                ),
                decision
                    .enforced_rules
                    .into_iter()
                    .map(|rule| rule.to_string())
                    .collect(),
            )
            .into());
        }

        // Resolve targets and save deliveries
        let mut target_sessions = Vec::new();
        if let Some(ref s_id) = args.target_session_id {
            target_sessions.push(s_id.clone());
        } else if let Some(ref a_id) = args.target_agent_id {
            // Only sessions that have not reached a terminal state can consume a
            // delivery. `list_sessions_for_agent` is the append-only historical
            // index — using it here reported every session the role had ever run
            // as a live recipient.
            if let Ok(sessions) = store.list_addressable_sessions_for_agent(a_id) {
                // A broadcast to one's own role must not loop back to the sender:
                // a self-delivered message would be injected into the very turn
                // that produced it.
                target_sessions.extend(
                    sessions
                        .into_iter()
                        .filter(|sid| sid.as_str() != sender_session_id.as_str()),
                );
            }

            if target_sessions.is_empty() {
                let mut exists = None;
                let mut status = "no_live_recipients";
                // State the actual rule, not "no active sessions": a session of
                // this role may well be running and still not be a recipient —
                // it has closed, or it is the sender's own session.
                let mut message = format!(
                    "Agent '{}' is installed but has no unfinished session able to receive the \
                     message (closed sessions and your own session are never recipients).",
                    a_id
                );

                // An alias-installed agent lives in `.gateway/revisions/<rev>`
                // with NO directory under `agents_dir`, so an ingest-dir-only
                // check reports it missing — which is how a fully installed,
                // inspectable agent once came back as `target_agent_not_found`.
                // Hence: alias first, ingest dir second.
                //
                // The ingest-dir read is deliberately retained here (#1136),
                // unlike the guardrail and grant paths. This branch decides
                // nothing — it only chooses which diagnostic an operator sees
                // after a message failed to route. Reading the on-disk bundle
                // makes that diagnostic strictly more informative: it can tell
                // "no such agent" from "there is a bundle but it is broken",
                // which is a distinction an operator needs and which no
                // promotion gate is protecting.
                let alias_known = store.get_agent_alias(a_id).ok().flatten().is_some();

                if alias_known {
                    exists = Some(true);
                } else if let Some(cfg) = config {
                    let repo = crate::agent::AgentRepository::new(cfg.agents_dir.clone());
                    match repo.load_unvetted_from_ingest_dir(a_id) {
                        Ok(_) => {
                            exists = Some(true);
                        }
                        Err(e) => {
                            let error_msg = e.to_string();
                            if error_msg.contains("not found") {
                                exists = Some(false);
                                status = "target_agent_not_found";
                                message = format!(
                                    "No installed agent found with id '{}' (checked both the alias registry and {}). agent_message requires an existing target agent with at least one unfinished session.",
                                    a_id,
                                    cfg.agents_dir.display()
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

        // Receiver-side consent (P-11.5, R-10.7 analogue).
        //
        // The sender-side ACL cannot express "this role may not be addressed by
        // you" — the boundary that matters for an evaluator, sentinel, auditor
        // or ombudsman whose verdict gates the sender. Both addressing modes
        // converge on `acl_target_agent_id`, so one check covers a direct send
        // and a role broadcast alike.
        //
        // Evaluated against the *receiving* agent's manifest: `policy` here
        // belongs to the sender and proves nothing about consent.
        //
        // Deliberately placed AFTER target resolution. Run earlier, it
        // pre-empted the existing diagnostics — a message to a nonexistent or
        // broken agent came back "consent unverifiable" instead of
        // `target_agent_not_found` / `target_agent_unavailable`, which is a
        // strictly worse answer to a different question. By here the recipient
        // is known to exist and to have at least one live session, so a load
        // failure really is a consent problem.
        if sender_agent_id != acl_target_agent_id {
            match load_receiving_agent_policy(&acl_target_agent_id, config, &store) {
                Ok(target_policy) => {
                    let consent = target_policy.accepts_peer_message_from(&sender_agent_id);
                    if !consent.is_allowed() {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "status": "recipient_refuses_peer_messages",
                            "target_agent_id": acl_target_agent_id,
                            "recipients_count": 0,
                            "message": format!(
                                "Agent '{}' does not accept peer messages from '{}' \
                                 (its manifest declares `messaging.accepts_from`). Nothing was \
                                 queued. A role whose verdict gates other agents is not \
                                 addressable by the parties it judges — route through the \
                                 operator or the workflow instead.",
                                acl_target_agent_id, sender_agent_id
                            ),
                        })
                        .to_string());
                    }
                }
                Err(e) => {
                    // Fail closed. Consent is unverifiable, and the alternative
                    // is to deliver into a manifest that may well be the one
                    // refusing. The sender is told plainly rather than getting
                    // a `delivered` it cannot trust.
                    return Ok(serde_json::json!({
                        "ok": false,
                        "status": "recipient_consent_unverifiable",
                        "target_agent_id": acl_target_agent_id,
                        "recipients_count": 0,
                        "message": format!(
                            "Could not load agent '{}' to check whether it accepts peer \
                             messages, so nothing was queued: {}",
                            acl_target_agent_id, e
                        ),
                    })
                    .to_string());
                }
            }
        }

        let target_pattern = if let Some(ref s_id) = args.target_session_id {
            format!("session:{}", s_id)
        } else {
            format!("agent:{}", args.target_agent_id.as_ref().unwrap())
        };

        let msg_id = autonoetic_types::id_format::short_random_id("msg-");

        let record = crate::scheduler::gateway_store::AgentMessageRecord {
            message_id: msg_id.clone(),
            sender_session_id: sender_session_id.clone(),
            sender_agent_id: sender_agent_id.clone(),
            target_pattern: target_pattern.clone(),
            message: args.message.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            // Stamp the payload with the sender's accumulated egress taint
            // (RFC §5.5): the recipient labels the ingested message with it, so
            // a tainted sender can't hand content to a remote-pinned sibling.
            egress_label: run_context.and_then(|rc| rc.egress_taint.clone()),
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

/// Load the *receiving* agent's manifest so its `messaging.accepts_from` can
/// be evaluated.
///
/// Alias/revision store first, ingest directory second — the same ordering the
/// broadcast diagnostic below already uses, and for the same #1136 reason: an
/// alias-installed agent has NO directory under `agents_dir`, so an
/// ingest-only read reports a fully installed agent as missing. Here that
/// would mean refusing every message to it.
///
/// The store is authoritative whenever an alias exists, which is always true
/// of an agent with a live session — it had to resolve a revision to run. The
/// ingest read is the fallback for bundles that were never promoted (dev and
/// test workspaces seed `agents_dir` directly); it can only supply a manifest
/// the store did not have, never override one it did.
fn load_receiving_agent_policy(
    agent_id: &str,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    store: &crate::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<PolicyEngine> {
    let config = config.ok_or_else(|| {
        anyhow::anyhow!("gateway config is unavailable, so recipient consent cannot be checked")
    })?;
    let gateway_dir = crate::execution::gateway_root_dir(config);
    let repo = crate::agent::AgentRepository::from_config(config);
    match repo.get_sync_from_store(agent_id, &gateway_dir, Some(store)) {
        Ok(loaded) => Ok(PolicyEngine::new(loaded.manifest)),
        Err(store_err) => match repo.load_unvetted_from_ingest_dir(agent_id) {
            Ok(loaded) => Ok(PolicyEngine::new(loaded.manifest)),
            Err(ingest_err) => Err(anyhow::anyhow!(
                "no promoted revision ({store_err}) and no readable bundle in the ingest \
                 directory ({ingest_err})"
            )),
        },
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AgentSpawnTool));
    registry.register(Box::new(AgentDiscoverTool));
    registry.register(Box::new(AgentListTool));
    registry.register(Box::new(AgentMessageTool));
}

/// Resolve the SKILL.md whose `io.accepts` contract governs a spawn.
///
/// Alias spawns (installed agents) read the live agent dir; candidate-revision
/// spawns (smoke tests, `revision_id = Some`) read the pinned revision dir —
/// the candidate is not installed yet, so the live dir is absent or stale.
///
/// Returns `None` when `revision_id` is not a safe single path component:
/// `PathBuf::join` honors absolute paths and `..` segments, so an unvalidated
/// caller-supplied revision id could escape the revisions directory and make
/// the gateway read an arbitrary SKILL.md from elsewhere on disk. Unlike the
/// execution path (which resolves revision records via the store first), this
/// helper touches the filesystem directly and must fail closed.
pub(crate) fn spawn_target_skill_path(
    agents_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    agent_id: &str,
    revision_id: Option<&str>,
) -> Option<std::path::PathBuf> {
    match revision_id {
        Some(rev_id)
            if !rev_id.is_empty()
                && rev_id != "."
                && rev_id != ".."
                && !rev_id.contains('/')
                && !rev_id.contains('\\') =>
        {
            Some(crate::agent::agent_revision_dir(gateway_dir, agent_id, rev_id).join("SKILL.md"))
        }
        Some(_) => None,
        None => Some(agents_dir.join(agent_id).join("SKILL.md")),
    }
}

/// Resolve a spawn target's declared egress output floor (#971) — the bundle's
/// own output restriction, surfaced on the session room's spawn row so an
/// operator can see that a delegation went to a local-only bundle. Reads the
/// same SKILL.md the schema-enforcement path resolves, so the floor matches the
/// exact revision being spawned (candidate or active). `None` when the manifest
/// is absent/unreadable or declares no floor — the floor is advisory operator
/// legibility, not enforcement, so a load failure degrades to "unrestricted"
/// rather than erroring.
pub(crate) fn resolve_target_output_label(
    agents_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    agent_id: &str,
    revision_id: Option<&str>,
) -> Option<autonoetic_types::egress::NamedEgressLabel> {
    let path = spawn_target_skill_path(agents_dir, gateway_dir, agent_id, revision_id)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let (manifest, _body) = crate::runtime::parser::SkillParser::parse(&content).ok()?;
    manifest.egress.and_then(|e| e.output_label)
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
mod promotion_role_derivation_tests {
    use super::fill_promotion_role_if_missing;

    #[test]
    fn derives_role_from_target_agent_when_gate_required() {
        let mut meta = serde_json::json!({
            "require_promotion_record": true,
            "promotion_artifact_ref": "ar.abc123"
        });
        fill_promotion_role_if_missing(&mut meta, "unit_test_runner.default");
        assert_eq!(meta["promotion_role"], "unit_test_runner");

        let mut meta = serde_json::json!({ "require_promotion_record": true });
        fill_promotion_role_if_missing(&mut meta, "auditor.default");
        assert_eq!(meta["promotion_role"], "auditor");

        let mut meta = serde_json::json!({ "require_promotion_record": true });
        fill_promotion_role_if_missing(&mut meta, "static_evaluator.default");
        assert_eq!(meta["promotion_role"], "static_evaluator");

        let mut meta = serde_json::json!({ "require_promotion_record": true });
        fill_promotion_role_if_missing(&mut meta, "sealed_evaluator.default");
        assert_eq!(meta["promotion_role"], "sealed_evaluator");
    }

    #[test]
    fn preserves_explicit_promotion_role() {
        let mut meta = serde_json::json!({
            "require_promotion_record": true,
            "promotion_role": "auditor"
        });
        fill_promotion_role_if_missing(&mut meta, "unit_test_runner.default");
        assert_eq!(meta["promotion_role"], "auditor");
    }

    #[test]
    fn derives_when_role_is_null_or_empty() {
        let mut meta = serde_json::json!({
            "require_promotion_record": true,
            "promotion_role": null
        });
        fill_promotion_role_if_missing(&mut meta, "auditor.default");
        assert_eq!(meta["promotion_role"], "auditor");

        let mut meta = serde_json::json!({
            "require_promotion_record": true,
            "promotion_role": ""
        });
        fill_promotion_role_if_missing(&mut meta, "auditor.default");
        assert_eq!(meta["promotion_role"], "auditor");
    }

    #[test]
    fn no_op_without_gate_requirement() {
        let mut meta = serde_json::json!({ "delegated_role": "coder" });
        fill_promotion_role_if_missing(&mut meta, "unit_test_runner.default");
        assert!(meta.get("promotion_role").is_none());
    }

    #[test]
    fn no_op_for_non_gate_agent() {
        let mut meta = serde_json::json!({ "require_promotion_record": true });
        fill_promotion_role_if_missing(&mut meta, "coder.default");
        assert!(meta.get("promotion_role").is_none());
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

    #[test]
    fn skill_path_for_alias_spawn_reads_live_agent_dir() {
        let path = spawn_target_skill_path(
            std::path::Path::new("/data/agents"),
            std::path::Path::new("/data/runtime"),
            "weather-forecast",
            None,
        )
        .expect("alias spawn path");
        assert_eq!(
            path,
            std::path::Path::new("/data/agents/weather-forecast/SKILL.md")
        );
    }

    #[test]
    fn skill_path_for_revision_spawn_reads_pinned_revision_dir() {
        let path = spawn_target_skill_path(
            std::path::Path::new("/data/agents"),
            std::path::Path::new("/data/runtime"),
            "weather-forecast",
            Some("rev_sha256:abc123"),
        )
        .expect("revision spawn path");
        // Resolved under the configured runtime dir, not under `agents_dir` —
        // the two are siblings and neither is derived from the other.
        assert_eq!(
            path,
            std::path::Path::new(
                "/data/runtime/revisions/agents/weather-forecast/rev_sha256:abc123/SKILL.md"
            )
        );
    }

    #[test]
    fn skill_path_rejects_revision_id_path_traversal() {
        for evil in ["..", ".", "../escape", "rev/../../escape", "/abs/path", "rev\\..\\win"] {
            assert!(
                spawn_target_skill_path(
                    std::path::Path::new("/data/agents"),
                    std::path::Path::new("/data/runtime"),
                    "a",
                    Some(evil)
                )
                .is_none(),
                "revision_id {evil:?} must be rejected"
            );
        }
        // `..` as a substring of an ordinary single component is not traversal.
        assert!(
            spawn_target_skill_path(
                std::path::Path::new("/data/agents"),
                std::path::Path::new("/data/runtime"),
                "a",
                Some("rev_.._x")
            )
            .is_some()
        );
    }

    #[test]
    fn resolve_target_output_label_reads_the_declared_floor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents_dir = tmp.path();
        let gateway_dir = tmp.path().join("runtime");
        let agent_dir = agents_dir.join("email.reader");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("SKILL.md"),
            r#"---
name: "email-reader"
description: "reads mail"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "email.reader"
      name: "Email"
      description: "reads mail"
    egress:
      output_label: local_only
---
# Email
"#,
        )
        .unwrap();
        assert_eq!(
            resolve_target_output_label(agents_dir, &gateway_dir, "email.reader", None),
            Some(autonoetic_types::egress::NamedEgressLabel::LocalOnly),
            "the declared local_only floor resolves for the spawn-row marker (#971)"
        );
    }

    #[test]
    fn resolve_target_output_label_none_when_no_floor_or_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents_dir = tmp.path();
        let gateway_dir = tmp.path().join("runtime");
        // No SKILL.md at all ⇒ None (advisory legibility, not an error).
        assert_eq!(
            resolve_target_output_label(agents_dir, &gateway_dir, "ghost.agent", None),
            None
        );
        let agent_dir = agents_dir.join("plain.agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("SKILL.md"),
            r#"---
name: "plain"
metadata:
  autonoetic:
    agent:
      id: "plain.agent"
      name: "Plain"
      description: "no floor"
---
# Plain
"#,
        )
        .unwrap();
        assert_eq!(
            resolve_target_output_label(agents_dir, &gateway_dir, "plain.agent", None),
            None,
            "a bundle declaring no floor is unrestricted ⇒ None"
        );
    }
}
