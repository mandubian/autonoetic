//! Internal JSON-RPC 2.0 Router.

use crate::execution::{
    gateway_actor_id, init_gateway_causal_logger, sha256_hex, GatewayExecutionService, SpawnResult,
};
use crate::runtime::workbench_return::prepare_return_to_agent_wakeup;
use crate::scheduler::append_task_board_entry;
use crate::tracing::{EventScope, SessionId, TraceSession};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::unsatisfied_dependencies;
use autonoetic_types::task_board::{TaskBoardEntry, TaskStatus};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::future::Future;
use std::sync::Arc;

/// Tracks async event.ingest results for polling via `session.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncIngestResult {
    pub session_id: String,
    pub status: AsyncIngestStatus,
    pub assistant_reply: Option<String>,
    pub workflow_note: Option<String>,
    pub artifacts: Vec<serde_json::Value>,
    pub shared_knowledge: Vec<serde_json::Value>,
    pub error: Option<String>,
    /// Constitutional rule/right IDs the refusal/termination enforced (e.g.
    /// `P-2.25`). Empty unless `status == Failed` with an attributed cause —
    /// lets an async client polling `session.status` learn *which* clause
    /// refused, matching the `error.data.enforced_rules` of the sync path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enforced_rules: Vec<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AsyncIngestStatus {
    Processing,
    Completed,
    SuspendedApproval,
    SuspendedUserInput,
    SuspendedChildWait,
    Failed,
}

type AsyncResultMap = Arc<tokio::sync::Mutex<std::collections::HashMap<String, AsyncIngestResult>>>;

#[derive(Debug)]
enum IngressType {
    Spawn {
        agent_id: String,
        source_agent_id: Option<String>,
        message: String,
        metadata: Option<serde_json::Value>,
    },
    Ingest {
        target_agent_id: String,
        source_agent_id: Option<String>,
        event_type: String,
        message: String,
        metadata: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: String,
    // Provide either result or error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    pub fn success(id: String, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: String, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Like [`error`](Self::error) but attaches the constitutional rule/right IDs
    /// the refusal enforced to `error.data` (`{ "enforced_rules": [...] }`), so a
    /// client is told *which* clause blocked it — not just a prose message. Empty
    /// `enforced_rules` ⇒ identical to `error`.
    pub fn error_with_rules(
        id: String,
        code: i32,
        message: impl Into<String>,
        enforced_rules: Vec<String>,
    ) -> Self {
        let data = if enforced_rules.is_empty() {
            None
        } else {
            Some(serde_json::json!({ "enforced_rules": enforced_rules }))
        };
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

type ProcessedSignalSet = Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

#[derive(Clone)]
pub struct JsonRpcRouter {
    config: Arc<GatewayConfig>,
    execution: Arc<GatewayExecutionService>,
    async_results: AsyncResultMap,
    /// Signal IDs already ingested — prevents at-least-once pump retries from
    /// triggering duplicate agent turns.
    processed_signal_ids: ProcessedSignalSet,
}

impl JsonRpcRouter {
    pub fn new(
        config: GatewayConfig,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        crate::constitution_digest::initialize_constitution(&config).unwrap_or_else(|e| {
            panic!(
                "failed to initialize configured constitution artifacts (source='{}', lock='{}'): {}",
                config.constitution.source_path.display(),
                config.constitution.lock_path.display(),
                e
            )
        });
        let config_dir = config
            .agents_dir
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let persona = crate::config::load_persona(&config, config_dir);
        let execution = Arc::new(GatewayExecutionService::new_with_persona(
            config.clone(),
            gateway_store,
            persona,
        ));
        Self {
            config: Arc::new(config),
            execution,
            async_results: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            processed_signal_ids: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
        }
    }

    pub fn execution_service(&self) -> Arc<GatewayExecutionService> {
        self.execution.clone()
    }

    async fn execute_agent_request(
        &self,
        ingress: IngressType,
        session_id: String,
    ) -> Result<(SpawnResult, Option<TraceSession>), (String, Vec<String>, Option<TraceSession>)> {
        let (
            action_name,
            agent_id,
            source_agent_id,
            message,
            event_type_for_inbox,
            event_type_str,
            metadata_for_trace,
        ) = match &ingress {
            IngressType::Spawn {
                agent_id,
                source_agent_id,
                message,
                metadata,
            } => {
                let kickoff = match metadata {
                    Some(value) => format!("{}\n\nDelegation metadata: {}", message, value),
                    None => message.clone(),
                };
                (
                    "agent_spawn",
                    agent_id.clone(),
                    source_agent_id.clone(),
                    kickoff,
                    None,
                    None,
                    metadata.clone(),
                )
            }
            IngressType::Ingest {
                target_agent_id,
                source_agent_id,
                event_type,
                message,
                metadata,
            } => {
                // Gateway-authored signal text that is already addressed to the
                // agent passes through verbatim; wrapping it in the
                // `Gateway event type: ... / Message: ... / Metadata: ...`
                // envelope would only add noise.
                let raw_signal_passthrough = metadata.as_ref().is_some_and(|value| {
                    if value.get("signal_delivered") != Some(&serde_json::Value::Bool(true)) {
                        return false;
                    }
                    // Prose notices declare themselves by signal type (e.g. the
                    // `agent_message` wake-up notice); JSON payloads are
                    // recognised by their `type` field.
                    if value.get("signal_type").and_then(|value| value.as_str())
                        == Some("agent_message")
                    {
                        return true;
                    }
                    serde_json::from_str::<serde_json::Value>(message)
                        .ok()
                        .is_some_and(|parsed| {
                            parsed.get("type").and_then(|value| value.as_str())
                                == Some("child_state_notification")
                        })
                });
                let kickoff = if raw_signal_passthrough {
                    message.clone()
                } else {
                    match metadata {
                        Some(metadata) => format!(
                            "Gateway event type: {}\nMessage: {}\nMetadata: {}",
                            event_type, message, metadata
                        ),
                        None => {
                            format!("Gateway event type: {}\nMessage: {}", event_type, message)
                        }
                    }
                };
                (
                    "event.ingest",
                    target_agent_id.clone(),
                    source_agent_id.clone(),
                    kickoff,
                    Some(event_type.clone()),
                    Some(event_type.clone()),
                    metadata.clone(),
                )
            }
        };

        let causal_logger = match init_gateway_causal_logger(self.config.as_ref()) {
            Ok(logger) => logger,
            Err(e) => {
                return Err((
                    format!(
                        "{} failed: unable to initialize gateway causal logger: {}",
                        action_name, e
                    ),
                    Vec::new(),
                    None,
                ));
            }
        };

        let mut trace_session = TraceSession::create_with_session_id(
            SessionId::from_string(session_id.clone()),
            Arc::new(causal_logger),
            gateway_actor_id(),
            EventScope::Request,
        );

        let requested_data = match (&ingress, &metadata_for_trace) {
            (
                IngressType::Spawn {
                    agent_id,
                    source_agent_id,
                    message,
                    metadata,
                },
                _,
            ) => serde_json::json!({
                "agent_id": agent_id,
                "source_agent_id": source_agent_id,
                "session_id": session_id,
                "message_len": message.len(),
                "message_sha256": sha256_hex(message),
                "metadata_sha256": metadata.as_ref().map(|v| sha256_hex(&v.to_string())),
            }),
            (
                IngressType::Ingest {
                    target_agent_id,
                    source_agent_id,
                    event_type,
                    message,
                    metadata,
                },
                _,
            ) => serde_json::json!({
                "event_type": event_type,
                "target_agent_id": target_agent_id,
                "source_agent_id": source_agent_id,
                "session_id": session_id,
                "message_len": message.len(),
                "message_sha256": sha256_hex(message),
                "metadata_sha256": metadata.as_ref().and_then(|v| serde_json::to_string(v).ok()).as_ref().map(|v| sha256_hex(v)),
            }),
        };

        let _ = trace_session.log_requested(action_name, Some(requested_data));

        let result = match self
            .spawn_agent_once(
                &agent_id,
                &message,
                &session_id,
                source_agent_id.as_deref(),
                false,
                event_type_for_inbox.as_deref(),
                metadata_for_trace.as_ref(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Preserve any constitutional rule IDs the refusal carried so the
                // RPC error can surface them to the client (instead of prose only).
                let enforced_rules = e
                    .downcast_ref::<autonoetic_types::tool_error::tagged::Tagged>()
                    .map(|t| t.enforced_rules().to_vec())
                    .unwrap_or_default();
                return Err((e.to_string(), enforced_rules, Some(trace_session)));
            }
        };

        // Background signal already sent in execution layer if needed (result.should_signal_background)

        let completed_data = match (&ingress, &event_type_str) {
            (
                IngressType::Spawn {
                    source_agent_id,
                    metadata,
                    ..
                },
                _,
            ) => serde_json::json!({
                "agent_id": result.agent_id,
                "source_agent_id": source_agent_id,
                "assistant_reply_len": result.assistant_reply.as_ref().map(|s| s.len()).unwrap_or(0),
                "assistant_reply_sha256": result.assistant_reply.as_ref().map(|s| sha256_hex(s)),
                "metadata_sha256": metadata.as_ref().map(|v| sha256_hex(&v.to_string())),
            }),
            (
                IngressType::Ingest {
                    target_agent_id,
                    source_agent_id,
                    event_type,
                    ..
                },
                _,
            ) => serde_json::json!({
                "event_type": event_type,
                "target_agent_id": target_agent_id,
                "source_agent_id": source_agent_id,
                "assistant_reply_len": result.assistant_reply.as_ref().map(|s| s.len()).unwrap_or(0),
                "assistant_reply_sha256": result.assistant_reply.as_ref().map(|s| sha256_hex(s)),
            }),
        };

        let _ = trace_session.log_completed(action_name, None, Some(completed_data));

        Ok((result, None))
    }

    /// Transition async_results entries from `SuspendedApproval` to `Processing`
    /// for the given session (and optionally its root session).
    ///
    /// This is the **single** place that clears a stale `SuspendedApproval`
    /// status after an approval has been granted.  Every approval path
    /// (JSON-RPC, inline TUI, interaction.answer) must call this instead of
    /// manipulating the map directly.
    async fn transition_async_to_processing(
        &self,
        session_id: &str,
        root_session_id: Option<&str>,
    ) {
        let mut map = self.async_results.lock().await;
        let now = chrono::Utc::now().to_rfc3339();
        for sid in std::iter::once(session_id)
            .chain(root_session_id.filter(|r| *r != session_id))
        {
            if let Some(entry) = map.get_mut(sid) {
                if entry.status == AsyncIngestStatus::SuspendedApproval {
                    entry.status = AsyncIngestStatus::Processing;
                    entry.completed_at = None;
                    entry.started_at = now.clone();
                    tracing::debug!(
                        target: "router",
                        session_id = %sid,
                        "Transitioned async_results from SuspendedApproval to Processing"
                    );
                }
            }
        }
    }

    /// Update an existing async_results entry with the final spawn result.
    ///
    /// Called by both the async and sync `event.ingest` paths after the
    /// planner finishes so that `session.status` reflects reality.
    fn apply_spawn_result_to_async_entry(
        entry: &mut AsyncIngestResult,
        spawn_result: &SpawnResult,
    ) {
        let status = if spawn_result.suspended_for_approval.is_some() {
            AsyncIngestStatus::SuspendedApproval
        } else if spawn_result.suspended_for_user_input {
            AsyncIngestStatus::SuspendedUserInput
        } else if spawn_result.suspended_for_child_wait {
            AsyncIngestStatus::SuspendedChildWait
        } else {
            AsyncIngestStatus::Completed
        };
        entry.status = status;
        entry.assistant_reply = spawn_result.assistant_reply.clone();
        entry.workflow_note = spawn_result.workflow_note.clone();
        entry.artifacts = spawn_result
            .artifacts
            .iter()
            .map(|a| serde_json::to_value(a).unwrap_or_default())
            .collect();
        entry.shared_knowledge = spawn_result
            .shared_knowledge
            .iter()
            .map(|k| serde_json::to_value(k).unwrap_or_default())
            .collect();
        entry.completed_at = if matches!(entry.status, AsyncIngestStatus::SuspendedChildWait) {
            None
        } else {
            Some(chrono::Utc::now().to_rfc3339())
        };
    }

    /// Check whether a plan step's `depends_on` are all Completed.
    /// Returns `Some(error_message)` if the spawn should be blocked, or `None` if it's allowed.
    ///
    /// Enforcement is opt-in: only activates when the spawn's `metadata` contains
    /// a `step_id` field. Spawns without `step_id` are not checked (backwards-compatible).
    fn check_plan_step_dependencies(
        &self,
        metadata: &Option<serde_json::Value>,
        session_id: &str,
    ) -> Option<String> {
        let metadata = metadata.as_ref()?;
        let step_id = metadata.get("step_id")?.as_str()?;
        let root_session_id = session_id.split('/').next().unwrap_or(session_id);

        let store = self.execution.gateway_store()?;

        // Find the latest approved plan for this root session.
        let plans = store.list_latest_plan_frames_for_root(root_session_id).ok()?;
        let plan = plans.first()?;

        // Verify the step exists in the plan (reject unknown step_ids).
        let step_exists = plan.steps.iter().any(|s| s.step_id == step_id);
        if !step_exists {
            return Some(format!(
                "Spawn metadata references step_id `{}` which does not exist in the approved plan `{}` (v{}). Remove step_id from metadata or use a valid step_id.",
                step_id, plan.plan_id, plan.version,
            ));
        }

        let unsatisfied = unsatisfied_dependencies(plan, step_id);
        if unsatisfied.is_empty() {
            return None;
        }

        let deps_desc: Vec<String> = unsatisfied
            .iter()
            .map(|(id, status)| format!("`{}` ({})", id, status.as_str()))
            .collect();
        Some(format!(
            "Plan step `{}` depends on {} which {} not completed. Complete the dependency step(s) before spawning for this step.",
            step_id,
            deps_desc.join(", "),
            if unsatisfied.len() == 1 { "is" } else { "are" },
        ))
    }

    /// `skill.crystallize_from_session` — operator-triggered skill
    /// crystallization on a specific session (`/crystallize`, #818).
    ///
    /// Lives in its own method rather than inline in [`Self::dispatch`]'s match:
    /// every arm's locals share that one function's stack frame, and adding this
    /// handler inline overflowed the stack of debug-build test threads (#882
    /// regression). Handlers of this size belong in methods.
    fn handle_skill_crystallize_from_session(
        &self,
        req_id: String,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
            // Operator-triggered skill crystallization on a specific session
            // (`/crystallize` in the session room, #818). Same shape as
            // curation.run_for_session below: resolve the agent, take its
            // singleton slot, enqueue into the session's own workflow so the
            // verdict lands in the timeline the operator is watching. All
            // constitution/approval gates stay in effect — the crystallizer
            // only proposes and delegates, and every route it takes ends at
            // a Candidate revision behind the standard promotion gates.
            let params: CrystallizeRunParams = match serde_json::from_value(params) {
                Ok(v) => v,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32602,
                        format!("Invalid params for skill.crystallize_from_session: {}", e),
                    );
                }
            };
            let store = match self.execution.gateway_store() {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        "Gateway store not available".to_string(),
                    );
                }
            };
            let config = self.execution.config();

            let crystallizer_ref = match crate::runtime::tools::resolve_target_to_agent_ref(
                "skill-crystallizer.default",
                store.as_ref(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        format!(
                            "Could not resolve skill-crystallizer.default (is the agent \
                             bundle installed?): {e}"
                        ),
                    );
                }
            };

            let workflow = match crate::scheduler::ensure_workflow_for_root_session(
                &config,
                Some(store.as_ref()),
                &params.root_session_id,
                Some("skill-crystallizer.default"),
            ) {
                Ok(w) => w,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        format!("Failed to resolve workflow for session: {e}"),
                    );
                }
            };
            let workflow_id = workflow.workflow_id.clone();
            let task_id = format!(
                "crystallize-{}-{}",
                &params.root_session_id,
                &uuid::Uuid::new_v4().to_string()[..8]
            );
            let child_session_id = format!("crystallize-child-{}", &params.root_session_id);
            let now_rfc = chrono::Utc::now().to_rfc3339();

            // The crystallizer is singleton=true. Take the slot before
            // enqueueing, exactly as agent.spawn does, so a repeated
            // /crystallize inside one tick window does not double-run it.
            let is_singleton = crate::runtime::tools::agent::target_agent_is_singleton(
                &config.agents_dir,
                &crystallizer_ref.agent_id,
            );
            let mut acquired_singleton_slot = false;
            if is_singleton {
                match store.acquire_singleton_slot(
                    &workflow_id,
                    &crystallizer_ref.agent_id,
                    Some(&crystallizer_ref.revision_id),
                    &task_id,
                ) {
                    Ok(Some(existing_task_id)) => {
                        return JsonRpcResponse::success(
                            req_id.clone(),
                            serde_json::json!({
                                "task_id": existing_task_id,
                                "workflow_id": workflow_id,
                                "session_id": params.root_session_id,
                                "status": "deduplicated",
                            }),
                        );
                    }
                    Ok(None) => acquired_singleton_slot = true,
                    Err(e) => {
                        tracing::warn!(
                            target: "crystallization",
                            workflow_id = %workflow_id,
                            agent_id = %crystallizer_ref.agent_id,
                            error = %e,
                            "Failed to acquire singleton slot for crystallizer; enqueueing anyway"
                        );
                    }
                }
            }

            // Matches the crystallizer's "Input (from spawn message)"
            // section. focus_notes is null rather than absent when the
            // operator gave none, so the agent reads the field uniformly.
            let focus_notes = params.focus_notes.clone();
            let message = serde_json::json!({
                "session_ids": [&params.root_session_id],
                "focus_notes": focus_notes,
            })
            .to_string();

            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task_id.clone(),
                workflow_id: workflow_id.clone(),
                agent_id: format!(
                    "{}@{}",
                    crystallizer_ref.agent_id, crystallizer_ref.revision_id
                ),
                message,
                child_session_id,
                parent_session_id: params.root_session_id.clone(),
                source_agent_id: "operator".to_string(),
                metadata: Some(serde_json::json!({
                    "manual_crystallization": true,
                    "root_session_id": &params.root_session_id,
                })),
                join_group: None,
                blocks_planner: false,
                enqueued_at: now_rfc.clone(),
                credential_bindings: vec![],
            };
            if let Err(e) =
                crate::scheduler::enqueue_task(&config, Some(store.as_ref()), &queued)
            {
                // Release the slot we took, or the next /crystallize would
                // dedup to a task that was never queued — wedging the
                // command until the workflow is cleaned up. Reachable
                // today: enqueue refuses on an emergency-stopped workflow.
                if acquired_singleton_slot {
                    if let Err(rel) =
                        store.release_singleton_slot_by_task_id(&workflow_id, &task_id)
                    {
                        tracing::warn!(
                            target: "crystallization",
                            workflow_id = %workflow_id,
                            task_id = %task_id,
                            error = %rel,
                            "Failed to release singleton slot after enqueue failure"
                        );
                    }
                }
                return JsonRpcResponse::error(
                    req_id.clone(),
                    -32000,
                    format!("Failed to enqueue crystallization task: {e}"),
                );
            }

            if let Err(e) = crate::scheduler::append_workflow_event(
                &config,
                Some(store.as_ref()),
                &autonoetic_types::workflow::WorkflowEventRecord {
                    event_id: format!("wevt-{}", &task_id),
                    workflow_id: workflow_id.clone(),
                    event_type: "crystallization.triggered".to_string(),
                    task_id: Some(task_id.clone()),
                    agent_id: Some(crystallizer_ref.agent_id.clone()),
                    payload: serde_json::json!({
                        "session_id": &params.root_session_id,
                        "focus_notes": focus_notes,
                        "manual": true,
                    }),
                    occurred_at: now_rfc,
                },
            ) {
                tracing::warn!(
                    target: "crystallization",
                    workflow_id = %workflow_id,
                    task_id = %task_id,
                    error = %e,
                    "Failed to append crystallization.triggered event"
                );
            }

            tracing::info!(
                target: "crystallization",
                root_session_id = %params.root_session_id,
                workflow_id = %workflow_id,
                task_id = %task_id,
                has_focus_notes = focus_notes.is_some(),
                "Manual crystallization triggered"
            );

            JsonRpcResponse::success(
                req_id.clone(),
                serde_json::json!({
                    "task_id": task_id,
                    "workflow_id": workflow_id,
                    "session_id": params.root_session_id,
                }),
            )
    }

    /// `evolution.list_pending` — the standing view of in-flight skill work
    /// (`/skills` in the session room, #818): proposals, the decisions recorded
    /// against them, and the Candidate revisions the promotion gate is holding.
    ///
    /// The assembly lives in `crate::evolution_view` so it can be unit-tested
    /// against a store without a router, and so this dispatch frame stays small
    /// (#884).
    fn handle_evolution_list_pending(
        &self,
        req_id: String,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(200) as usize;
        let store = match self.execution.gateway_store() {
            Some(s) => s,
            None => {
                return JsonRpcResponse::error(
                    req_id,
                    -32000,
                    "Gateway store not available".to_string(),
                );
            }
        };
        match crate::evolution_view::pending_view(store.as_ref(), limit) {
            Ok(view) => JsonRpcResponse::success(req_id, view),
            Err(e) => JsonRpcResponse::error(
                req_id,
                -32000,
                format!("Failed to assemble the pending evolution view: {e}"),
            ),
        }
    }

    /// `curation.run_for_session` — operator-triggered memory curation on a
    /// specific session (`/curate`). Extracted for the same stack-frame reason
    /// as the crystallization handler above.
    fn handle_curation_run_for_session(
        &self,
        req_id: String,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
            // Operator-triggered memory curation on a specific session,
            // with optional focus notes that steer the curator. Fires the
            // memory-curator agent the normal way (enqueue -> drain ->
            // spawn), so all constitution/approval gates remain in effect.
            // Spawned into the session's own workflow so the result is
            // visible in the timeline the operator is watching.
            let params: CurationRunParams = match serde_json::from_value(params) {
                Ok(v) => v,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32602,
                        format!("Invalid params for curation.run_for_session: {}", e),
                    );
                }
            };
            let store = match self.execution.gateway_store() {
                Some(s) => s,
                None => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        "Gateway store not available".to_string(),
                    );
                }
            };
            let config = self.execution.config();

            // Resolve the memory-curator agent + revision.
            let curator_ref = match crate::runtime::tools::resolve_target_to_agent_ref(
                "memory-curator.default",
                store.as_ref(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        format!(
                            "Could not resolve memory-curator.default (is the agent bundle \
                             installed?): {e}"
                        ),
                    );
                }
            };

            // Spawn into the session's own workflow so curation output lands
            // in the operator's timeline.
            let workflow = match crate::scheduler::ensure_workflow_for_root_session(
                &config,
                Some(store.as_ref()),
                &params.root_session_id,
                Some("memory-curator.default"),
            ) {
                Ok(w) => w,
                Err(e) => {
                    return JsonRpcResponse::error(
                        req_id.clone(),
                        -32000,
                        format!("Failed to resolve workflow for session: {e}"),
                    );
                }
            };
            let workflow_id = workflow.workflow_id.clone();
            // Use the shared `short_random_id` helper (the codebase's
            // canonical replacement for the `&Uuid::new_v4().to_string()[..8]`
            // idiom — see autonoetic_types::id_format). The `curate-` prefix
            // on task_id is load-bearing: `maybe_emit_workflow_timeline` in
            // workflow_store.rs uses `task_id.starts_with("curate-")` to
            // discriminate manual `/curate` tasks from cron tasks when
            // mirroring terminal events onto the root timeline.
            let task_id = autonoetic_types::id_format::short_random_id("curate-");
            // Slash-form child session id (NOT "curate-child-{root}"). The slash
            // is load-bearing: content_store::root_session_id() is pure
            // `session_id.split('/').next()`, so timeline queries that filter by
            // root_session_id only attribute the curator's events to the root if
            // the child id starts with "{root}/". Without the slash the curator
            // is its own root and the operator sees nothing on the timeline
            // after /curate — the session-3739f831 visibility gap. This matches
            // agent_spawn's "{root}/{agent}-{rand}" convention at agent.rs:711
            // exactly (same helper, same shape) so resolve()/workflow_wait
            // behave consistently across both spawn paths.
            let child_session_id = format!(
                "{}/{}-{}",
                &params.root_session_id,
                curator_ref.agent_id,
                autonoetic_types::id_format::short_random_id("")
            );
            let now_rfc = chrono::Utc::now().to_rfc3339();

            // Memory-curator is singleton=true. The normal agent.spawn path
            // acquires a singleton slot before enqueueing to dedup; this
            // operator RPC must do the same, otherwise repeated /curate
            // within a tick window can enqueue duplicate curator tasks.
            //
            // NOTE: the singleton key is (workflow_id, agent_id, revision_id)
            // — never the child session id. Cron-fired curators run in a
            // separate sched-{job_id} workflow and do NOT consult this slot,
            // so a cron curation and a /curate can run concurrently. That is
            // tolerable today because /curate's focus_notes distinguishes a
            // manual operator inspection from a routine scheduled sweep.
            // Cross-workflow mutual exclusion is a separate follow-up.
            let is_singleton = crate::runtime::tools::agent::target_agent_is_singleton(
                &config.agents_dir,
                &curator_ref.agent_id,
            );
            let mut acquired_singleton_slot = false;
            if is_singleton {
                match store.acquire_singleton_slot(
                    &workflow_id,
                    &curator_ref.agent_id,
                    Some(&curator_ref.revision_id),
                    &task_id,
                ) {
                    Ok(Some(existing_task_id)) => {
                        // A curator is already active in this workflow —
                        // return the existing task rather than duplicating.
                        return JsonRpcResponse::success(
                            req_id.clone(),
                            serde_json::json!({
                                "task_id": existing_task_id,
                                "workflow_id": workflow_id,
                                "session_id": params.root_session_id,
                                "status": "deduplicated",
                            }),
                        );
                    }
                    Ok(None) => acquired_singleton_slot = true,
                    Err(e) => {
                        tracing::warn!(
                            target: "curation",
                            workflow_id = %workflow_id,
                            agent_id = %curator_ref.agent_id,
                            error = %e,
                            "Failed to acquire singleton slot for curator; enqueueing anyway"
                        );
                    }
                }
            }

            // The curator parses its spawn message as JSON (see its SKILL
            // "Input (from spawn message)" section). focus_notes is null
            // when absent so the agent can treat it uniformly.
            let focus_notes = params.focus_notes.clone();
            let message = serde_json::json!({
                "session_ids": [&params.root_session_id],
                "max_sessions": 50,
                "focus_notes": focus_notes,
            })
            .to_string();

            let queued = autonoetic_types::workflow::QueuedTaskRun {
                task_id: task_id.clone(),
                workflow_id: workflow_id.clone(),
                agent_id: format!(
                    "{}@{}",
                    curator_ref.agent_id, curator_ref.revision_id
                ),
                message,
                child_session_id: child_session_id.clone(),
                parent_session_id: params.root_session_id.clone(),
                source_agent_id: "operator".to_string(),
                metadata: Some(serde_json::json!({
                    "manual_curation": true,
                    "root_session_id": &params.root_session_id,
                })),
                join_group: None,
                blocks_planner: false,
                enqueued_at: now_rfc.clone(),
                credential_bindings: vec![],
            };
            if let Err(e) = crate::scheduler::enqueue_task(
                &config,
                Some(store.as_ref()),
                &queued,
            ) {
                // Same wedge as the crystallization handler above: a slot
                // held for a task that was never queued would make every
                // later /curate dedup to a phantom run.
                if acquired_singleton_slot {
                    if let Err(rel) =
                        store.release_singleton_slot_by_task_id(&workflow_id, &task_id)
                    {
                        tracing::warn!(
                            target: "curation",
                            workflow_id = %workflow_id,
                            task_id = %task_id,
                            error = %rel,
                            "Failed to release singleton slot after enqueue failure"
                        );
                    }
                }
                return JsonRpcResponse::error(
                    req_id.clone(),
                    -32000,
                    format!("Failed to enqueue curation task: {e}"),
                );
            }

            // Wire the curator into the root session's lineage — mirrors
            // agent_spawn at agent.rs:798-854. Without these three calls the
            // curator's child session (now slash-form) is not registered as a
            // child of the root for content-store visibility, spawn-lineage
            // queries, or the root's causal chain. The slash-form id alone
            // fixes timeline attribution; these calls close the remaining
            // gaps so resolve()/workflow_wait behave consistently with
            // agent_spawn.
            let gw_dir = crate::execution::gateway_root_dir(&config);
            if let Ok(content_store) =
                crate::runtime::content_store::ContentStore::new(&gw_dir)
            {
                if let Err(e) = content_store
                    .set_root_session(&child_session_id, &params.root_session_id)
                {
                    tracing::warn!(
                        target: "curation",
                        error = %e,
                        parent_session = %params.root_session_id,
                        child_session = %child_session_id,
                        "Failed to set root session for curator child"
                    );
                }
            }
            if let Err(e) = store.upsert_session_spawn_lineage(
                &child_session_id,
                &params.root_session_id,
                &params.root_session_id,
                0,
                &curator_ref.agent_id,
                &now_rfc,
            ) {
                tracing::warn!(
                    target: "curation",
                    error = %e,
                    child_session_id = %child_session_id,
                    "Failed to record curator spawn lineage"
                );
            }
            crate::scheduler::workflow_causal::mirror_orchestration_event(
                &config,
                &params.root_session_id,
                "workflow.task.spawned",
                autonoetic_types::causal_chain::EntryStatus::Success,
                serde_json::json!({
                    "workflow_id": &workflow_id,
                    "task_id": &task_id,
                    "target_agent_id": &curator_ref.agent_id,
                    "child_session_id": &child_session_id,
                    "parent_session_id": &params.root_session_id,
                    "source_agent_id": "operator",
                    "trigger": "manual_curation",
                }),
            );

            if let Err(e) = crate::scheduler::append_workflow_event(
                &config,
                Some(store.as_ref()),
                &autonoetic_types::workflow::WorkflowEventRecord {
                    event_id: format!("wevt-{}", &task_id),
                    workflow_id: workflow_id.clone(),
                    event_type: "curation.triggered".to_string(),
                    task_id: Some(task_id.clone()),
                    agent_id: Some(curator_ref.agent_id.clone()),
                    payload: serde_json::json!({
                        "session_id": &params.root_session_id,
                        "focus_notes": focus_notes,
                        "manual": true,
                    }),
                    occurred_at: now_rfc,
                },
            ) {
                tracing::warn!(
                    target: "curation",
                    workflow_id = %workflow_id,
                    task_id = %task_id,
                    error = %e,
                    "Failed to append curation.triggered event"
                );
            }

            tracing::info!(
                target: "curation",
                root_session_id = %params.root_session_id,
                workflow_id = %workflow_id,
                task_id = %task_id,
                has_focus_notes = focus_notes.is_some(),
                "Manual curation triggered"
            );

            JsonRpcResponse::success(
                req_id.clone(),
                serde_json::json!({
                    "task_id": task_id,
                    "workflow_id": workflow_id,
                    "session_id": params.root_session_id,
                }),
            )
    }

    /// JSON-RPC method routing.
    ///
    /// **Keep new arms thin — put anything substantial in a method.** Every
    /// arm's locals live in *this* function's stack frame, whether or not that
    /// arm runs, so the frame is the sum of all 62 arms. It is already large
    /// enough that adding one ~200-line handler inline overflowed the 2 MiB
    /// stack of the worker threads a default `cargo test` uses (#882 regression,
    /// fixed by extracting the handler; see #884 for the remaining oversized
    /// arms). CI does not catch this class of failure: `--test-threads=1` runs
    /// tests on the 8 MiB main thread, where the frame still fits.
    pub async fn dispatch(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        tracing::debug!("Dispatching JSON-RPC method: {}", req.method);

        match req.method.as_str() {
            "ping" => JsonRpcResponse::success(req.id, serde_json::json!("pong")),
            "gateway.info" => JsonRpcResponse::success(
                req.id,
                serde_json::json!({
                    "gateway_version": env!("CARGO_PKG_VERSION"),
                    "constitution_digest": crate::constitution_digest::constitution_digest().as_ref(),
                    "constitution_version": crate::constitution_digest::constitution_version().as_ref(),
                    "constitution_format_version": crate::constitution_digest::constitution_format_version(),
                }),
            ),
            "constitution.get" => {
                let params: autonoetic_types::constitution::ConstitutionGetParams =
                    if req.params.is_null() {
                        Default::default()
                    } else {
                        match serde_json::from_value(req.params) {
                            Ok(v) => v,
                            Err(e) => {
                                return JsonRpcResponse::error(
                                    req.id,
                                    -32602,
                                    format!("Invalid params for constitution.get: {}", e),
                                );
                            }
                        }
                    };
                let result = crate::constitution_digest::constitution_profile(params.include_text);
                JsonRpcResponse::success(
                    req.id,
                    serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({})),
                )
            }
            "constitution.resolve_proposal" => {
                // O-6 (Decider Obligations, §O of the 2026.07.08 amendment):
                // every Ri-0.8 proposal is owed a recorded decision. Mirrors
                // admin.escalation_resolve's shape; no bidirectional projection
                // to resolve (proposals don't project into another gate table).
                #[derive(Deserialize)]
                struct ResolveProposalParams {
                    proposal_id: String,
                    decided_by: String,
                    status: String,
                    reason: Option<String>,
                }
                let params: ResolveProposalParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for constitution.resolve_proposal: {}", e),
                        );
                    }
                };
                if params.proposal_id.trim().is_empty() || params.decided_by.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "proposal_id and decided_by must not be empty",
                    );
                }
                if !crate::scheduler::gateway_store::constitutional_proposals::PROPOSAL_DECISION_STATUSES
                    .contains(&params.status.as_str())
                {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        format!(
                            "Invalid status '{}'; expected one of {}",
                            params.status,
                            crate::scheduler::gateway_store::constitutional_proposals::PROPOSAL_DECISION_STATUSES
                                .join(", "),
                        ),
                    );
                }
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for constitution.resolve_proposal",
                    );
                };
                match store.get_constitutional_proposal(&params.proposal_id) {
                    Ok(None) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Proposal '{}' not found", params.proposal_id),
                        );
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to look up proposal: {}", e),
                        );
                    }
                    Ok(Some(_)) => {}
                }
                match store.decide_constitutional_proposal(
                    &params.proposal_id,
                    &params.status,
                    &params.decided_by,
                    params.reason.as_deref(),
                ) {
                    Ok(true) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "proposal_id": params.proposal_id,
                            "status": params.status,
                            "decided_by": params.decided_by,
                        }),
                    ),
                    Ok(false) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Proposal '{}' not found", params.proposal_id),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to resolve proposal: {}", e),
                    ),
                }
            }
            "constitution.list_pending_proposals" => {
                // Visibility counterpart to constitution.resolve_proposal.
                // Deliberately NOT folded into operator_pending's per-root
                // aggregation: a ConstitutionalProposal carries no
                // root_session_id (it is a gateway-global concern — any
                // agent may propose, any operator may decide), so it does not
                // fit collect_pending_for_root's root-scoped model without a
                // scoping decision the RFC (#359/#399) has not made. This is
                // a separate, honestly-global list, mirroring
                // wiki.proposals_pending's shape.
                #[derive(Deserialize)]
                struct ListPendingProposalsParams {
                    #[serde(default)]
                    status: Option<String>,
                    #[serde(default)]
                    limit: Option<usize>,
                }
                let params: ListPendingProposalsParams = if req.params.is_null() {
                    ListPendingProposalsParams { status: None, limit: None }
                } else {
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for constitution.list_pending_proposals: {}", e),
                            );
                        }
                    }
                };
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for constitution.list_pending_proposals",
                    );
                };
                let status_filter = params.status.as_deref().unwrap_or("pending");
                match store.list_constitutional_proposals(
                    Some(status_filter),
                    None,
                    params.limit.unwrap_or(50),
                ) {
                    Ok(proposals) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({ "proposals": proposals }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to list proposals: {}", e),
                    ),
                }
            }
            "anomaly.resolve" => {
                // O-7 (future obligation, issue #770 part C.1): every
                // anomaly flag is owed a recorded decision. Mirrors
                // "constitution.resolve_proposal", plus a decider-obligation
                // motivation requirement on terminal decisions (mirroring
                // scheduler::approval::enforce_decider_motivation's
                // presence-only semantics — never judges the reason's
                // quality, only requires one be present).
                #[derive(Deserialize)]
                struct ResolveFlagParams {
                    flag_id: String,
                    decided_by: String,
                    status: String,
                    reason: Option<String>,
                }
                let params: ResolveFlagParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for anomaly.resolve: {}", e),
                        );
                    }
                };
                if params.flag_id.trim().is_empty() || params.decided_by.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "flag_id and decided_by must not be empty",
                    );
                }
                if !crate::scheduler::gateway_store::anomaly_flags::FLAG_DECISION_STATUSES
                    .contains(&params.status.as_str())
                {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        format!(
                            "Invalid status '{}'; expected one of {}",
                            params.status,
                            crate::scheduler::gateway_store::anomaly_flags::FLAG_DECISION_STATUSES
                                .join(", "),
                        ),
                    );
                }
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for anomaly.resolve",
                    );
                };
                let flag = match store.get_anomaly_flag(&params.flag_id) {
                    Ok(None) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Anomaly flag '{}' not found", params.flag_id),
                        );
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to look up anomaly flag: {}", e),
                        );
                    }
                    Ok(Some(f)) => f,
                };

                let is_terminal =
                    crate::scheduler::gateway_store::anomaly_flags::FLAG_TERMINAL_DECISION_STATUSES
                        .contains(&params.status.as_str());
                let config = self.execution.config();
                if is_terminal && config.decider_obligations.enabled {
                    let has_reason = params
                        .reason
                        .as_deref()
                        .map(|r| !r.trim().is_empty())
                        .unwrap_or(false);
                    if !has_reason {
                        emit_anomaly_decider_obligation_event(
                            &store,
                            &flag,
                            &params.decided_by,
                            &params.status,
                            "refused",
                        );
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!(
                                "§O decider obligation (O-7): recording anomaly flag '{}' as '{}' \
                                 requires a motivation. Provide a non-empty reason and retry.",
                                params.flag_id, params.status
                            ),
                        );
                    }
                }

                match store.decide_anomaly_flag(
                    &params.flag_id,
                    &params.status,
                    &params.decided_by,
                    params.reason.as_deref(),
                ) {
                    Ok(true) => {
                        if is_terminal && config.decider_obligations.enabled {
                            emit_anomaly_decider_obligation_event(
                                &store,
                                &flag,
                                &params.decided_by,
                                &params.status,
                                "satisfied",
                            );
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "flag_id": params.flag_id,
                                "status": params.status,
                                "decided_by": params.decided_by,
                            }),
                        )
                    }
                    Ok(false) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Anomaly flag '{}' not found", params.flag_id),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to resolve anomaly flag: {}", e),
                    ),
                }
            }
            "anomaly.list_pending" => {
                // Visibility counterpart to anomaly.resolve, mirroring
                // "constitution.list_pending_proposals".
                #[derive(Deserialize)]
                struct ListPendingFlagsParams {
                    #[serde(default)]
                    status: Option<String>,
                    #[serde(default)]
                    limit: Option<usize>,
                }
                let params: ListPendingFlagsParams = if req.params.is_null() {
                    ListPendingFlagsParams { status: None, limit: None }
                } else {
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for anomaly.list_pending: {}", e),
                            );
                        }
                    }
                };
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for anomaly.list_pending",
                    );
                };
                let status_filter = params.status.as_deref().unwrap_or("pending");
                match store.list_anomaly_flags(Some(status_filter), None, params.limit.unwrap_or(50))
                {
                    Ok(flags) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({ "flags": flags }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to list anomaly flags: {}", e),
                    ),
                }
            }
            "interaction.answer" => {
                let params: crate::interaction_answer::InteractionAnswerParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for interaction.answer: {}", e),
                            );
                        }
                    };
                let execution = self.execution.clone();
                match crate::interaction_answer::answer_and_orchestrate_resume(&execution, params)
                    .await
                {
                    Ok(out) => {
                        {
                            let mut map = self.async_results.lock().await;
                            if let Some(sid) = &out.session_id {
                                map.remove(sid);
                            }
                            if let Some(root) = &out.root_session_id {
                                if root != out.session_id.as_deref().unwrap_or("") {
                                    map.remove(root);
                                }
                            }
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::to_value(out).unwrap_or_else(|_| serde_json::json!({})),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(req.id, -32000, e.to_string()),
                }
            }
            "interaction.resolve_and_answer" => {
                let params: crate::interaction_answer::InteractionResolveAndAnswerParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for interaction.resolve_and_answer: {}", e),
                            );
                        }
                    };
                let execution = self.execution.clone();
                match crate::interaction_answer::resolve_and_answer(&execution, params).await {
                    Ok(out) => {
                        {
                            let mut map = self.async_results.lock().await;
                            if let Some(sid) = &out.session_id {
                                map.remove(sid);
                            }
                            if let Some(root) = &out.root_session_id {
                                if root != out.session_id.as_deref().unwrap_or("") {
                                    map.remove(root);
                                }
                            }
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::to_value(out).unwrap_or_else(|_| serde_json::json!({})),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(req.id, -32000, e.to_string()),
                }
            }
            "agent_spawn" => {
                let params: AgentSpawnParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for agent.spawn: {}", e),
                        );
                    }
                };
                let session_id = params
                    .session_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let agent_id = params.agent_id.clone();

                // Enforce plan step depends_on ordering (#664).
                // When the spawn carries a step_id in metadata, verify that all
                // declared dependencies are Completed before allowing the spawn.
                if let Some(block_msg) =
                    self.check_plan_step_dependencies(&params.metadata, &session_id)
                {
                    return JsonRpcResponse::error(req.id, -32000, block_msg);
                }

                let ingress = IngressType::Spawn {
                    agent_id: params.agent_id.clone(),
                    source_agent_id: params.source_agent_id.clone(),
                    message: params.message.clone(),
                    metadata: params.metadata.clone(),
                };

                // Box::pin so the large `execute_with_history` future lives on
                // the heap, not in `dispatch`'s poll frame (#884/#916 stack
                // budget — see the sibling call in the event.ingest arm).
                match Box::pin(self.execute_agent_request(ingress, session_id.clone()))
                    .await
                {
                    Ok((result, _trace_session)) => {
                        if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                            let task_status = delegation_task_status_for_spawn(&result);
                            let _ = append_delegation_task_entry(
                                self.config.as_ref(),
                                source_agent_id,
                                &agent_id,
                                "agent_spawn",
                                task_status,
                                Some(serde_json::json!({
                                    "session_id": result.session_id.clone(),
                                    "assistant_reply": result.assistant_reply.clone(),
                                    "workflow_note": result.workflow_note.clone(),
                                    "artifacts": result.artifacts.clone(),
                                    "shared_knowledge": result.shared_knowledge.clone(),
                                    "delegation_metadata": params.metadata.clone(),
                                })),
                            );
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "agent_id": result.agent_id,
                                "session_id": result.session_id,
                                "assistant_reply": result.assistant_reply,
                                "workflow_note": result.workflow_note,
                                "artifacts": result.artifacts,
                                "shared_knowledge": result.shared_knowledge,
                                "llm_usage": result.llm_usage,
                            }),
                        )
                    }
                    Err((e, enforced_rules, maybe_trace_session)) => {
                        if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                            let _ = append_delegation_task_entry(
                                self.config.as_ref(),
                                source_agent_id,
                                &agent_id,
                                "agent_spawn",
                                TaskStatus::Failed,
                                Some(serde_json::json!({
                                    "error": e.clone(),
                                })),
                            );
                        }
                        if let Some(mut trace_session) = maybe_trace_session {
                            let _ = trace_session.log_failed(
                                "agent_spawn",
                                &e,
                                Some(serde_json::json!({
                                    "agent_id": agent_id.clone(),
                                    "source_agent_id": params.source_agent_id.clone(),
                                })),
                            );
                        }
                        JsonRpcResponse::error_with_rules(
                            req.id,
                            -32000,
                            format!("agent.spawn failed: {}", e),
                            enforced_rules,
                        )
                    }
                }
            }
            "skill.crystallize_from_session" => {
                self.handle_skill_crystallize_from_session(req.id, req.params)
            }
            "curation.run_for_session" => {
                self.handle_curation_run_for_session(req.id, req.params)
            }
            "evolution.list_pending" => {
                self.handle_evolution_list_pending(req.id, req.params)
            }
            "event.ingest" => {
                let params: EventIngestParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for event.ingest: {}", e),
                        );
                    }
                };

                // ── Idempotency guard for pump-delivered signals ──
                // The notification pump retries on timeout, but the gateway may
                // have already processed the original TCP request.  Deduplicate
                // by the signal's request_id to prevent spurious agent turns.
                if let Some(meta) = params.metadata.as_ref() {
                    if meta.get("signal_delivered") == Some(&serde_json::Value::Bool(true)) {
                        if let Some(signal_req_id) = meta
                            .get("signal_request_id")
                            .and_then(|v| v.as_str())
                            .or_else(|| meta.get("approval_request_id").and_then(|v| v.as_str()))
                        {
                            let mut seen = self.processed_signal_ids.lock().unwrap();
                            if !seen.insert(signal_req_id.to_string()) {
                                tracing::info!(
                                    target: "gateway.router",
                                    signal_request_id = %signal_req_id,
                                    "Duplicate signal delivery detected — returning success (idempotent no-op)"
                                );
                                return JsonRpcResponse::success(
                                    req.id,
                                    serde_json::json!({
                                        "status": "already_processed",
                                        "signal_request_id": signal_req_id,
                                    }),
                                );
                            }
                        }
                    }
                }

                let mut session_id = params
                    .session_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let event_type = params.event_type.clone();
                let mut ingest_rerouted_from_child = false;
                let mut reroute_lead: Option<String> = None;
                if params.event_type.trim() == "chat" {
                    match crate::scheduler::workflow_store::reroute_chat_ingest_for_active_workflow_child_session(
                        self.config.as_ref(),
                        None,
                        &session_id,
                    ) {
                        Ok(Some(reroute)) => {
                            tracing::info!(
                                target: "gateway.router",
                                from_session = %session_id,
                                root_session = %reroute.root_session_id,
                                workflow_id = %reroute.workflow_id,
                                "event.ingest chat rerouted from child workflow session to root planner session"
                            );
                            ingest_rerouted_from_child = true;
                            reroute_lead = reroute.lead_agent_id;
                            session_id = reroute.root_session_id;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("event.ingest routing failed: {}", e),
                            );
                        }
                    }
                }
                let explicit_target = if ingest_rerouted_from_child {
                    reroute_lead.as_deref()
                } else {
                    params.target_agent_id.as_deref()
                };
                let target_agent_id =
                    match self.resolve_ingest_target_agent_id(&session_id, explicit_target) {
                        Ok(value) => value,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("event.ingest routing failed: {}", e),
                            );
                        }
                    };

                // Operator's own message onto the canonical timeline (#405) — so
                // any channel (room, Discord) shows both sides of the conversation,
                // not just the agent's replies. Written gateway-side and once,
                // *after* routing is confirmed (so a routing failure leaves no
                // "ghost" message that was never dispatched) but before dispatch,
                // so the operator line precedes the agent's response even under
                // async_mode.
                if params.event_type.trim() == "chat" && !params.message.trim().is_empty() {
                    if let Some(store) = self.execution.gateway_store() {
                        let redacted =
                            crate::log_redaction::redact_text_for_logs(&params.message);
                        if let Some(event) =
                            crate::runtime::session_timeline::ingest_chat_timeline_event(
                                &session_id,
                                params.source_agent_id.as_deref(),
                                &redacted,
                                params.metadata.as_ref(),
                            )
                        {
                            if let Err(e) = store.create_live_digest_event(&event) {
                                tracing::debug!(
                                    target: "session_timeline",
                                    error = %e,
                                    event_type = %event.event_type,
                                    "ingest chat timeline emit failed"
                                );
                            }
                        }
                    }
                }

                let ingress = IngressType::Ingest {
                    target_agent_id: target_agent_id.clone(),
                    source_agent_id: params.source_agent_id.clone(),
                    event_type: params.event_type.clone(),
                    message: params.message.clone(),
                    metadata: params.metadata.clone(),
                };

                if params.async_mode {
                    let async_results = self.async_results.clone();
                    let router = self.clone();
                    let session_id_clone = session_id.clone();
                    let target_agent_id_clone = target_agent_id.clone();
                    let event_type_clone = event_type.clone();
                    let source_agent_id = params.source_agent_id.clone();
                    let config = self.config.clone();

                    {
                        let mut map = async_results.lock().await;
                        map.insert(
                            session_id_clone.clone(),
                            AsyncIngestResult {
                                session_id: session_id_clone.clone(),
                                status: AsyncIngestStatus::Processing,
                                assistant_reply: None,
                                workflow_note: None,
                                artifacts: Vec::new(),
                                shared_knowledge: Vec::new(),
                                error: None,
                                enforced_rules: Vec::new(),
                                started_at: chrono::Utc::now().to_rfc3339(),
                                completed_at: None,
                            },
                        );
                    }

                    tokio::spawn(async move {
                        let result = router
                            .execute_agent_request(ingress, session_id_clone.clone())
                            .await;
                        let mut map = async_results.lock().await;
                        if let Some(entry) = map.get_mut(&session_id_clone) {
                            match result {
                                Ok((spawn_result, _)) => {
                                    if let Some(source) = source_agent_id {
                                        let task_status =
                                            delegation_task_status_for_spawn(&spawn_result);
                                        let _ = append_delegation_task_entry(
                                            config.as_ref(),
                                            &source,
                                            &target_agent_id_clone,
                                            "event.ingest",
                                            task_status,
                                            Some(serde_json::json!({
                                                "session_id": spawn_result.session_id.clone(),
                                                "assistant_reply": spawn_result.assistant_reply.clone(),
                                                "workflow_note": spawn_result.workflow_note.clone(),
                                                "artifacts": spawn_result.artifacts.clone(),
                                                "shared_knowledge": spawn_result.shared_knowledge.clone(),
                                                "event_type": event_type_clone,
                                            })),
                                        );
                                    }
                                    JsonRpcRouter::apply_spawn_result_to_async_entry(entry, &spawn_result);
                                }
                                Err((e, enforced_rules, _)) => {
                                    if let Some(source) = source_agent_id {
                                        let _ = append_delegation_task_entry(
                                            config.as_ref(),
                                            &source,
                                            &target_agent_id_clone,
                                            "event.ingest",
                                            TaskStatus::Failed,
                                            Some(serde_json::json!({
                                                "error": e.clone(),
                                                "event_type": event_type_clone,
                                                "enforced_rules": enforced_rules.clone(),
                                            })),
                                        );
                                    }
                                    entry.status = AsyncIngestStatus::Failed;
                                    entry.error = Some(e);
                                    entry.enforced_rules = enforced_rules;
                                    entry.completed_at = Some(chrono::Utc::now().to_rfc3339());
                                }
                            }
                        }
                    });

                    JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "event_type": event_type,
                            "target_agent_id": target_agent_id,
                            "session_id": session_id,
                            "status": "processing",
                            "message": "Request accepted. Poll session.status for result.",
                        }),
                    )
                } else {
                    // `Box::pin` the execute future so the (large)
                    // `execute_with_history` state machine lives on the heap,
                    // not inside `dispatch`'s poll frame — that frame is the sum
                    // of all 62 match arms against libtest's 2 MiB test thread
                    // (#884/#916), and embedding this future inline overflows it.
                    match Box::pin(self.execute_agent_request(ingress, session_id.clone()))
                        .await
                    {
                        Ok((result, _trace_session)) => {
                            if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                                let task_status = delegation_task_status_for_spawn(&result);
                                let _ = append_delegation_task_entry(
                                    self.config.as_ref(),
                                    source_agent_id,
                                    &target_agent_id,
                                    "event.ingest",
                                    task_status,
                                    Some(serde_json::json!({
                                        "session_id": result.session_id.clone(),
                                        "assistant_reply": result.assistant_reply.clone(),
                                        "workflow_note": result.workflow_note.clone(),
                                        "artifacts": result.artifacts.clone(),
                                        "shared_knowledge": result.shared_knowledge.clone(),
                                        "event_type": event_type.clone(),
                                    })),
                                );
                            }
                            // Update async_results if this session has a polling
                            // entry (e.g. standalone approval resume via sync
                            // ApprovalResolved signal).
                            {
                                let mut map = self.async_results.lock().await;
                                if let Some(entry) = map.get_mut(&session_id) {
                                    Self::apply_spawn_result_to_async_entry(entry, &result);
                                }
                            }
                            JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({
                                    "event_type": event_type,
                                    "target_agent_id": target_agent_id,
                                    "session_id": result.session_id,
                                    "assistant_reply": result.assistant_reply,
                                    "workflow_note": result.workflow_note,
                                    "artifacts": result.artifacts,
                                    "shared_knowledge": result.shared_knowledge,
                                    "llm_usage": result.llm_usage,
                                }),
                            )
                        }
                        Err((e, enforced_rules, maybe_trace_session)) => {
                            if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                                let _ = append_delegation_task_entry(
                                    self.config.as_ref(),
                                    source_agent_id,
                                    &target_agent_id,
                                    "event.ingest",
                                    TaskStatus::Failed,
                                    Some(serde_json::json!({
                                        "error": e.clone(),
                                        "event_type": event_type.clone(),
                                    })),
                                );
                            }
                            if let Some(mut trace_session) = maybe_trace_session {
                                let _ = trace_session.log_failed(
                                    "event.ingest",
                                    &e,
                                    Some(serde_json::json!({
                                        "event_type": event_type.clone(),
                                        "target_agent_id": target_agent_id.clone(),
                                        "source_agent_id": params.source_agent_id.clone(),
                                    })),
                                );
                            }
                            JsonRpcResponse::error_with_rules(
                                req.id,
                                -32000,
                                format!("event.ingest failed: {}", e),
                                enforced_rules,
                            )
                        }
                    }
                }
            }

            "session.status" => {
                #[derive(Deserialize)]
                struct SessionStatusParams {
                    session_id: String,
                }
                let params: SessionStatusParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.status: {}", e),
                        );
                    }
                };

                let async_results = self.async_results.lock().await;
                if let Some(result) = async_results.get(&params.session_id) {
                    JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_else(|_| {
                            serde_json::json!({ "session_id": params.session_id, "status": "unknown" })
                        }),
                    )
                } else {
                    JsonRpcResponse::error(
                        req.id,
                        -32001,
                        format!("No async result found for session '{}'. The session may have been initiated with async_mode=false, or the session ID is incorrect.", params.session_id),
                    )
                }
            }

            "operator.activity.list" => {
                let params: autonoetic_types::operator_activity::OperatorActivityListParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for operator.activity.list: {}", e),
                            );
                        }
                    };
                if params.root_session_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "root_session_id is required".to_string(),
                    );
                }
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let min_severity = params
                    .min_severity
                    .as_deref()
                    .and_then(autonoetic_types::operator_activity::OperatorActivitySeverity::parse_str);
                let limit = params.limit.clamp(1, 200);
                match store.list_operator_activity(
                    &params.root_session_id,
                    params.after_activity_id.as_deref(),
                    limit,
                    min_severity,
                ) {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("operator.activity.list failed: {}", e),
                    ),
                }
            }

            "session.timeline.list" => {
                // Canonical Session Room timeline over the gateway API (#391) so
                // channels are clients, not direct store readers.
                let params: autonoetic_types::session_timeline::SessionTimelineListParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for session.timeline.list: {}", e),
                            );
                        }
                    };
                if params.root_session_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "root_session_id is required".to_string(),
                    );
                }
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                // Per the type contract: omitted floor ⇒ Normal; an invalid floor
                // is an error, not a silent "no filter".
                let min_altitude = match params.min_altitude.as_deref() {
                    None => autonoetic_types::session_timeline::Altitude::Normal,
                    Some(s) => match autonoetic_types::session_timeline::Altitude::parse_str(s) {
                        Some(a) => a,
                        None => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!(
                                    "invalid min_altitude '{}': expected detail | normal | attention | error",
                                    s
                                ),
                            );
                        }
                    },
                };
                let limit = params.limit.clamp(1, 500);
                match store.list_session_timeline(
                    &params.root_session_id,
                    params.after_event_id.as_deref(),
                    limit,
                    Some(min_altitude),
                    params.principal_id.as_deref(),
                ) {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.timeline.list failed: {}", e),
                    ),
                }
            }

            "scheduled_jobs.list" => {
                let params: autonoetic_types::scheduled_job::ScheduledJobsListParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for scheduled_jobs.list: {}", e),
                            );
                        }
                    };
                let status = match params.status.as_deref() {
                    None => None,
                    Some("active") => {
                        Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Active)
                    }
                    Some("paused") => {
                        Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Paused)
                    }
                    Some("cancelled") => {
                        Some(autonoetic_types::scheduled_job::ScheduledJobStatus::Cancelled)
                    }
                    Some(other) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!(
                                "Invalid status for scheduled_jobs.list: '{}' (expected active|paused|cancelled)",
                                other
                            ),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let limit = params.limit.clamp(1, 500) as usize;
                match store.list_scheduled_jobs(
                    params.owner_agent_id.as_deref(),
                    params.root_session_id.as_deref(),
                    status,
                    limit,
                ) {
                    Ok(jobs) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(
                            autonoetic_types::scheduled_job::ScheduledJobsListResult { jobs },
                        )
                        .unwrap_or_else(|_| serde_json::json!({"jobs": []})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("scheduled_jobs.list failed: {}", e),
                    ),
                }
            }

            "scheduled_jobs.trigger" => {
                // Manually fire a scheduled job now on the running gateway,
                // bypassing the cron schedule. Operator-only (shared-secret
                // auth on this channel); fires the agent the normal way
                // (enqueue -> drain -> spawn_agent_once), so all
                // constitution/approval gates remain in effect.
                let params: autonoetic_types::scheduled_job::ScheduledJobTriggerParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for scheduled_jobs.trigger: {}", e),
                            );
                        }
                    };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let job = match store.get_scheduled_job(&params.job_id) {
                    Ok(Some(j)) => j,
                    Ok(None) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Scheduled job '{}' not found", params.job_id),
                        );
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to load scheduled job: {}", e),
                        );
                    }
                };
                if job.status != autonoetic_types::scheduled_job::ScheduledJobStatus::Active {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!(
                            "Scheduled job '{}' is {}; resume it before triggering",
                            params.job_id, job.status
                        ),
                    );
                }

                // In-flight guard: refuse to overlap a prior fire (manual or
                // cron) for the same job unless `force` is set. The workflow
                // id is the stable `sched-{job_id}` used by every fire.
                if !params.force {
                    let workflow_id = format!("sched-{}", &params.job_id);
                    match store.inflight_task_for_workflow(&workflow_id) {
                        Ok(Some(existing_task_id)) => {
                            return JsonRpcResponse::success(
                                req.id,
                                serde_json::to_value(
                                    autonoetic_types::scheduled_job::ScheduledJobTriggerResult::TriggerSkipped {
                                        job_id: params.job_id.clone(),
                                        existing_task_id,
                                    },
                                )
                                .unwrap_or_else(|_| serde_json::json!({})),
                            );
                        }
                        Ok(None) => {}
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("In-flight check failed: {}", e),
                            );
                        }
                    }
                }

                let now = chrono::Utc::now();
                match crate::scheduler::enqueue_scheduled_job_fire(
                    &self.execution.config(),
                    store.as_ref(),
                    &job,
                    now,
                    /* manual */ true,
                    /* next_run_at_override */ None,
                ) {
                    Ok(event) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(
                            autonoetic_types::scheduled_job::ScheduledJobTriggerResult::Triggered { event },
                        )
                        .unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("scheduled_jobs.trigger failed: {}", e),
                    ),
                }
            }

            "session.list" => {
                // Discover existing root sessions so the operator can reload or
                // attach to one. Backed by `causal_events` (every gateway
                // action leaves a row) — `MAX(timestamp)` gives last activity.
                let params: autonoetic_types::session_timeline::SessionListParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for session.list: {}", e),
                            );
                        }
                    };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let limit = params.limit.clamp(1, 500) as i64;
                match store.list_recent_sessions(limit, params.agent_id.as_deref()) {
                    Ok(rows) => {
                        let entries: Vec<autonoetic_types::session_timeline::SessionListEntry> =
                            rows.into_iter()
                                .map(|(sid, agent_id, last_ts)| {
                                    autonoetic_types::session_timeline::SessionListEntry {
                                        root_session_id: sid,
                                        agent_id,
                                        last_active_at: last_ts,
                                    }
                                })
                                .collect();
                        // The agent filter is pushed into the store query, so the
                        // returned rows are already filtered and bounded by `limit`.
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::to_value(
                                autonoetic_types::session_timeline::SessionListResult {
                                    sessions: entries,
                                },
                            )
                            .unwrap_or_else(|_| serde_json::json!({"sessions": []})),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.list failed: {}", e),
                    ),
                }
            }

            "planframes.list_pending" => {
                let params: autonoetic_types::plan_frame::PlanFramesListPendingParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for planframes.list_pending: {}", e),
                            );
                        }
                    };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match crate::scheduler::pending_plan_frames_for_root(
                    store.as_ref(),
                    &params.root_session_id,
                ) {
                    Ok(plans) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(
                            autonoetic_types::plan_frame::PlanFramesListPendingResult { plans },
                        )
                        .unwrap_or_else(|_| serde_json::json!({"plans": []})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("planframes.list_pending failed: {}", e),
                    ),
                }
            }

            "planframes.get" => {
                let params: autonoetic_types::plan_frame::PlanFramesGetParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for planframes.get: {}", e),
                            );
                        }
                    };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let result = match params.version {
                    Some(v) => store.load_plan_frame_revision(&params.plan_id, v),
                    None => store.load_plan_frame(&params.plan_id),
                };
                match result {
                    Ok(Some(plan)) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(
                            autonoetic_types::plan_frame::PlanFramesGetResult { plan },
                        )
                        .unwrap_or_else(|_| serde_json::json!({"plan": null})),
                    ),
                    Ok(None) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        match params.version {
                            Some(v) => format!("Plan '{}' version {} not found", params.plan_id, v),
                            None => format!("Plan '{}' not found", params.plan_id),
                        },
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("planframes.get failed: {}", e),
                    ),
                }
            }

            "planframes.get_active" => {
                #[derive(serde::Deserialize)]
                struct PlanFramesGetActiveParams {
                    root_session_id: String,
                }
                let params: PlanFramesGetActiveParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for planframes.get_active: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let wf_id = match store.resolve_workflow_id(&params.root_session_id) {
                    Ok(Some(id)) => Some(id),
                    Ok(None) => {
                        crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
                            self.config.as_ref(),
                            &params.root_session_id,
                        )
                        .ok()
                        .flatten()
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to resolve workflow ID: {}", e),
                        );
                    }
                };

                let plan = if let Some(wf_id) = wf_id {
                    match store.load_active_plan_for_workflow(&wf_id) {
                        Ok(p) => p,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("Failed to load active plan: {}", e),
                            );
                        }
                    }
                } else {
                    None
                };

                JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({ "plan": plan }),
                )
            }

            "planframes.approve" => {
                let params: autonoetic_types::plan_frame::PlanFramesApproveParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for planframes.approve: {}", e),
                            );
                        }
                    };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let plan = match store.load_plan_frame(&params.plan_id) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("planframes.approve failed: plan not found"),
                        );
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("planframes.approve failed: {}", e),
                        );
                    }
                };
                let request_id = crate::runtime::tools::plan_frame::plan_approval_request_id(
                    &params.plan_id,
                    plan.version,
                );
                match crate::scheduler::approval::approve_request(
                    self.config.as_ref(),
                    Some(store.as_ref()),
                    &request_id,
                    &params.approved_by,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(_) => match store.load_plan_frame(&params.plan_id) {
                        Ok(Some(plan)) => JsonRpcResponse::success(
                            req.id,
                            serde_json::to_value(
                                autonoetic_types::plan_frame::PlanFramesApproveResult { plan },
                            )
                            .unwrap_or(serde_json::Value::Null),
                        ),
                        _ => JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "planframes.approve failed: plan disappeared after approval"
                                .to_string(),
                        ),
                    },
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("planframes.approve failed: {}", e),
                    ),
                }
            }

            "workbench.prepare_return_to_agent" => {
                #[derive(serde::Deserialize)]
                struct WorkbenchPrepareReturnParams {
                    root_session_id: String,
                    #[serde(default)]
                    force: bool,
                    #[serde(default)]
                    note: Option<String>,
                }
                let params: WorkbenchPrepareReturnParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for workbench.prepare_return_to_agent: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                let wf_id = match store.resolve_workflow_id(&params.root_session_id) {
                    Ok(Some(id)) => Some(id),
                    Ok(None) => {
                        crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
                            self.config.as_ref(),
                            &params.root_session_id,
                        )
                        .ok()
                        .flatten()
                    }
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to resolve workflow ID: {}", e),
                        );
                    }
                };

                let maybe_wb = if let Some(wf_id) = wf_id {
                    match store.load_active_workbench_for_workflow(&wf_id) {
                        Ok(wb) => wb,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("Failed to load active workbench: {}", e),
                            );
                        }
                    }
                } else {
                    None
                };

                let Some(wb) = maybe_wb else {
                    return JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({ "status": "no_workbench" }),
                    );
                };

                match prepare_return_to_agent_wakeup(
                    store.as_ref(),
                    &wb.workbench_id,
                    params.force,
                    params.note.as_deref(),
                ) {
                    crate::runtime::workbench_return::ReturnToAgentStatus::Refused { reason } => {
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "status": "refused",
                                "reason": reason,
                            }),
                        )
                    }
                    crate::runtime::workbench_return::ReturnToAgentStatus::Ready {
                        target_agent_id,
                        outbound_message,
                        metadata,
                    } => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "status": "ready",
                            "target_agent_id": target_agent_id,
                            "message": outbound_message,
                            "metadata": metadata,
                        }),
                    ),
                }
            }

            "wiki.proposals_pending" => {
                #[derive(serde::Deserialize)]
                struct WikiProposalsPendingParams {
                    #[serde(default)]
                    root_session_id: Option<String>,
                    #[serde(default)]
                    limit: Option<usize>,
                }
                let params: WikiProposalsPendingParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for wiki.proposals_pending: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "GatewayStore not available for wiki.proposals_pending",
                        );
                    }
                };
                let all = match store.get_pending_approvals() {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("wiki.proposals_pending failed: {}", e),
                        );
                    }
                };
                let proposals: Vec<_> = all
                    .into_iter()
                    .filter(|a| {
                        matches!(
                            a.action,
                            autonoetic_types::background::ScheduledAction::WikiProposal { .. }
                        )
                    })
                    .filter(|a| {
                        params.root_session_id.as_deref().map_or(true, |sid| {
                            a.root_session_id.as_deref() == Some(sid)
                                || a.session_id == sid
                        })
                    })
                    .take(params.limit.unwrap_or(50))
                    .collect();
                JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({ "proposals": proposals }),
                )
            }

            "wiki.list" => {
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let wiki_root = gateway_dir.join("wiki");
                match crate::runtime::tools::wiki::list_pages(
                    if wiki_root.exists() { Some(gateway_dir.as_path()) } else { None },
                ) {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_else(|_| serde_json::json!({"pages": []})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("wiki.list failed: {}", e),
                    ),
                }
            }

            "wiki.get" => {
                let params: autonoetic_types::wiki::WikiGetParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for wiki.get: {}", e),
                            );
                        }
                    };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let wiki_root = gateway_dir.join("wiki");
                match crate::runtime::tools::wiki::get_page(
                    if wiki_root.exists() { Some(gateway_dir.as_path()) } else { None },
                    &params.id,
                ) {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result)
                            .unwrap_or_else(|_| serde_json::json!({"error": "serialization failed"})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("wiki.get failed: {}", e),
                    ),
                }
            }

            "wiki.propose" => {
                #[derive(Deserialize)]
                struct WikiProposeParams {
                    id: String,
                    title: String,
                    content: String,
                    #[serde(default)]
                    tags: Vec<String>,
                    #[serde(default)]
                    session_id: Option<String>,
                    #[serde(default)]
                    turn_id: Option<String>,
                    #[serde(default)]
                    agent_id: Option<String>,
                }

                let params: WikiProposeParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for wiki.propose: {}", e),
                        );
                    }
                };

                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "GatewayStore not available".to_string(),
                        );
                    }
                };

                // Resolve agent_id from session binding or parameter
                let resolved_agent_id = if let Some(aid) = &params.agent_id {
                    aid.clone()
                } else if let Some(sid) = &params.session_id {
                    match store.get_session_agent_binding(sid) {
                        Ok(Some(binding)) => binding.agent_id,
                        _ => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                "Could not resolve agent_id from session; provide agent_id explicitly".to_string(),
                            );
                        }
                    }
                } else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "agent_id or session_id is required".to_string(),
                    );
                };

                let (manifest, _agent_dir) = match self.execution.load_agent_manifest(&resolved_agent_id) {
                    Ok(m) => m,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to load agent manifest for '{}': {}", resolved_agent_id, e),
                        );
                    }
                };

                let is_edit = crate::runtime::tools::wiki::resolve_wiki_dir(
                    Some(&crate::execution::gateway_root_dir(self.config.as_ref())),
                )
                .map(|dir| dir.join(format!("{}.md", &params.id)).exists())
                .unwrap_or(false);

                let gate_kind = crate::runtime::human_gate::GateKind::WikiProposal {
                    page_id: params.id.clone(),
                    title: params.title.clone(),
                    content: params.content.clone(),
                    tags: params.tags.clone(),
                    is_edit,
                    proposed_by_agent: manifest.agent.id.clone(),
                    proposed_by_session: params.session_id.clone().unwrap_or_else(|| "unknown".to_string()),
                };

                let gate = crate::runtime::human_gate::GateService::new(store);
                let gate_req = crate::runtime::human_gate::GateRequest {
                    kind: gate_kind,
                    manifest: &manifest,
                    session_id: params.session_id.as_deref(),
                    run_context: None,
                    config: Some(self.config.as_ref()),
                    context: crate::runtime::human_gate::DecisionContext::tier2(
                        format!(
                            "wiki {} \"{}\" ({})",
                            if is_edit { "edit" } else { "new" },
                            params.title,
                            params.id
                        ),
                        "agent proposes a wiki change for review",
                        "publishes agent-authored content to the wiki",
                        "Approve if the proposed wiki content is accurate and appropriate to publish; reject if it is inaccurate, low-quality, or out of scope",
                    ),
                    summary: format!("Wiki proposal: {}", params.title),
                    approval_ref: None,
                    request_id: None,
                    pre_validated: false,
                    cache_backfill: None,
                    turn_id: params.turn_id.as_deref(),
                };

                match gate.check(gate_req) {
                    Ok(result) => match result {
                        crate::runtime::human_gate::GateResult::Cleared { .. }
                        | crate::runtime::human_gate::GateResult::PolicyAllowed => {
                            JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({
                                    "ok": true,
                                    "id": params.id,
                                    "is_edit": is_edit,
                                    "status": "approved",
                                }),
                            )
                        }
                        crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                            JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({
                                    "ok": true,
                                    "id": params.id,
                                    "gate_id": gate_id,
                                    "is_edit": is_edit,
                                    "status": "pending",
                                    "proposed_at": chrono::Utc::now().to_rfc3339(),
                                }),
                            )
                        }
                        crate::runtime::human_gate::GateResult::Suspended { response_json, .. } => {
                            match serde_json::from_str::<serde_json::Value>(&response_json) {
                                Ok(mut resp) => {
                                    if let Some(obj) = resp.as_object_mut() {
                                        obj.insert("id".to_string(), serde_json::Value::String(params.id));
                                        obj.insert("is_edit".to_string(), serde_json::Value::Bool(is_edit));
                                    }
                                    JsonRpcResponse::success(req.id, resp)
                                }
                                Err(_) => JsonRpcResponse::success(
                                    req.id,
                                    serde_json::json!({
                                        "ok": true,
                                        "id": params.id,
                                        "gate_id": null,
                                        "is_edit": is_edit,
                                        "status": "pending",
                                    }),
                                ),
                            }
                        }
                    },
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("wiki.propose failed: {}", e),
                    ),
                }
            }

            "channel.bind" => {
                // #393 (P3.c): bind an external conversation (Discord thread,
                // WhatsApp chat) to a room so it survives reconnects and routes
                // replies back as Operator-seat events. Channels are API clients,
                // not direct store readers (#390).
                let params: autonoetic_types::channel::ChannelBindParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for channel.bind: {}", e),
                            );
                        }
                    };
                if params.channel.trim().is_empty()
                    || params.external_id.trim().is_empty()
                    || params.root_session_id.trim().is_empty()
                {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "channel, external_id, and root_session_id are required".to_string(),
                    );
                }
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match store.bind_channel(
                    &params.channel,
                    &params.external_id,
                    &params.root_session_id,
                ) {
                    Ok(binding) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(binding).unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("channel.bind failed: {}", e),
                    ),
                }
            }

            "channel.resolve" => {
                // #393 (P3.c): look up which room a conversation is bound to.
                let params: autonoetic_types::channel::ChannelResolveParams =
                    match serde_json::from_value(req.params) {
                        Ok(v) => v,
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Invalid params for channel.resolve: {}", e),
                            );
                        }
                    };
                if params.channel.trim().is_empty() || params.external_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "channel and external_id are required".to_string(),
                    );
                }
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match store.resolve_channel_binding(&params.channel, &params.external_id) {
                    Ok(binding) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(
                            autonoetic_types::channel::ChannelResolveResult { binding },
                        )
                        .unwrap_or_else(|_| serde_json::json!({})),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("channel.resolve failed: {}", e),
                    ),
                }
            }

            "session.approval_resolved" => {
                #[derive(Deserialize)]
                struct ApprovalResolvedParams {
                    session_id: String,
                    #[serde(default)]
                    root_session_id: Option<String>,
                }
                let params: ApprovalResolvedParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.approval_resolved: {}", e),
                        );
                    }
                };
                self.transition_async_to_processing(
                    &params.session_id,
                    params.root_session_id.as_deref(),
                ).await;
                JsonRpcResponse::success(req.id, serde_json::json!({ "ok": true }))
            }

            "root_session.emergency_stop" => {
                #[derive(Deserialize)]
                struct EmergencyStopParams {
                    root_session_id: String,
                    reason: String,
                    requested_by_type: String,
                    requested_by_id: String,
                    #[serde(default)]
                    trigger_kind: Option<String>,
                    #[serde(default)]
                    source_agent_id: Option<String>,
                    #[serde(default)]
                    notify_where_practical: Option<bool>,
                }
                let params: EmergencyStopParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for root_session.emergency_stop: {}", e),
                        );
                    }
                };
                let trigger = params
                    .trigger_kind
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "manual".to_string());
                match self
                    .execution
                    .emergency_stop_root_session_with_options(
                        &params.root_session_id,
                        &params.reason,
                        &params.requested_by_type,
                        &params.requested_by_id,
                        &trigger,
                        params.source_agent_id.as_deref(),
                        params.notify_where_practical.unwrap_or(false),
                    )
                    .await
                {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            "session.degrade" => {
                #[derive(Deserialize)]
                struct DegradeParams {
                    session_id: String,
                    reason: String,
                    #[serde(default)]
                    notify_where_practical: Option<bool>,
                }
                let params: DegradeParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.degrade: {}", e),
                        );
                    }
                };
                match self
                    .execution
                    .degrade_session_with_options(
                        &params.session_id,
                        &params.reason,
                        params.notify_where_practical.unwrap_or(true),
                    )
                    .await
                {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            "session.clear_degradation" => {
                #[derive(Deserialize)]
                struct ClearDegradeParams {
                    session_id: String,
                }
                let params: ClearDegradeParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.clear_degradation: {}", e),
                        );
                    }
                };
                match self
                    .execution
                    .clear_session_degradation(&params.session_id)
                    .await
                {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            "session.inference.get" => {
                #[derive(Deserialize)]
                struct InferenceGetParams {
                    session_id: String,
                    #[serde(default)]
                    agent_id: Option<String>,
                }
                let params: InferenceGetParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.inference.get: {}", e),
                        );
                    }
                };
                match self.execution.get_session_inference(
                    &params.session_id,
                    params.agent_id.as_deref(),
                ) {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            "session.inference.set" => {
                #[derive(Deserialize)]
                struct InferenceSetParams {
                    session_id: String,
                    #[serde(default)]
                    agent_id: Option<String>,
                    preset: String,
                    #[serde(default)]
                    reason: Option<String>,
                    #[serde(default = "default_operator_set_by")]
                    set_by: String,
                }
                fn default_operator_set_by() -> String {
                    "operator:rpc".to_string()
                }
                let params: InferenceSetParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.inference.set: {}", e),
                        );
                    }
                };
                match self.execution.set_session_inference_override(
                    &params.session_id,
                    params.agent_id.as_deref(),
                    &params.preset,
                    params.reason.as_deref(),
                    &params.set_by,
                ) {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            "session.inference.clear" => {
                #[derive(Deserialize)]
                struct InferenceClearParams {
                    session_id: String,
                    #[serde(default = "default_operator_clear_by")]
                    set_by: String,
                }
                fn default_operator_clear_by() -> String {
                    "operator:rpc".to_string()
                }
                let params: InferenceClearParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.inference.clear: {}", e),
                        );
                    }
                };
                match self
                    .execution
                    .clear_session_inference_override(&params.session_id, &params.set_by)
                {
                    Ok(v) => JsonRpcResponse::success(req.id, v),
                    Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
                }
            }

            // Session-scoped egress policy (RFC data-envelopes §5.4) — the
            // operator's "for this room, these sources stay local". Rules are
            // added to the operator-global `egress.rules` and can only restrict;
            // the record dies with the root session.
            //
            // Bodies live out of line (`#[inline(never)]` free functions): this
            // match is thousands of arms long and, in debug builds, every arm's
            // locals share one stack frame. Inlining a handful more param
            // structs here is enough to overflow the stack during server
            // bootstrap.
            "session.egress_policy.get" => handle_egress_policy_get(&self.execution, req),
            "session.egress_policy.set" => handle_egress_policy_set(&self.execution, req),
            "session.egress_policy.clear" => handle_egress_policy_clear(&self.execution, req),

            "session.envelope.propose" => {
                #[derive(Deserialize)]
                struct EnvelopeProposeParams {
                    root_session_id: String,
                    #[serde(default = "default_envelope_source")]
                    source: String,
                    #[serde(default)]
                    plan_id: Option<String>,
                    #[serde(default = "default_envelope_proposed_by")]
                    proposed_by: String,
                }
                fn default_envelope_source() -> String {
                    "operator".to_string()
                }
                fn default_envelope_proposed_by() -> String {
                    "operator".to_string()
                }
                let params: EnvelopeProposeParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.envelope.propose: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match crate::scheduler::propose_session_envelope(
                    store.as_ref(),
                    &params.root_session_id,
                    &params.source,
                    params.plan_id.as_deref(),
                    &params.proposed_by,
                ) {
                    Ok(proposal) => match store.discover_observed_hosts(&params.root_session_id) {
                        Ok(observed_hosts) => JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "proposal": proposal,
                                "observed_hosts": observed_hosts,
                            }),
                        ),
                        Err(e) => JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("discover_observed_hosts failed: {}", e),
                        ),
                    },
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.envelope.propose failed: {}", e),
                    ),
                }
            }

            "session.envelope.lock" => {
                #[derive(Deserialize)]
                struct EnvelopeLockParams {
                    envelope_id: i64,
                    #[serde(default = "default_envelope_locked_by")]
                    locked_by: String,
                }
                fn default_envelope_locked_by() -> String {
                    "operator".to_string()
                }
                let params: EnvelopeLockParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.envelope.lock: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match crate::scheduler::lock_session_envelope_operator(
                    store.as_ref(),
                    params.envelope_id,
                    &params.locked_by,
                ) {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_default(),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.envelope.lock failed: {}", e),
                    ),
                }
            }

            "session.envelope.revoke" => {
                #[derive(Deserialize)]
                struct EnvelopeRevokeParams {
                    envelope_id: i64,
                    #[serde(default = "default_envelope_revoked_by")]
                    revoked_by: String,
                }
                fn default_envelope_revoked_by() -> String {
                    "operator".to_string()
                }
                let params: EnvelopeRevokeParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.envelope.revoke: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match crate::runtime::session_envelope::revoke_session_envelope(
                    store.as_ref(),
                    params.envelope_id,
                    &params.revoked_by,
                ) {
                    Ok(Some(record)) => {
                        let promoted_agents = store
                            .find_promotions_by_envelope(params.envelope_id)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|(agent_id, promotion_id, created_at)| {
                                serde_json::json!({
                                    "agent_id": agent_id,
                                    "promotion_id": promotion_id,
                                    "created_at": created_at,
                                })
                            })
                            .collect::<Vec<_>>();
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "ok": true,
                                "envelope_id": params.envelope_id,
                                "was_locked": record.locked_at.is_some(),
                                "root_session_id": record.root_session_id,
                                "agents_promoted_under_envelope": promoted_agents,
                            }),
                        )
                    },
                    Ok(None) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session envelope {} not found", params.envelope_id),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.envelope.revoke failed: {}", e),
                    ),
                }
            }

            "session.envelope.list" => {
                #[derive(Deserialize)]
                struct EnvelopeListParams {
                    root_session_id: String,
                }
                let params: EnvelopeListParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.envelope.list: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match crate::scheduler::list_session_envelopes(store.as_ref(), &params.root_session_id)
                {
                    Ok(result) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(result).unwrap_or_default(),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("session.envelope.list failed: {}", e),
                    ),
                }
            }

            // Suspend an agent: block new sessions while leaving in-flight
            // sessions running. An operator decision, decoupled from envelope
            // revocation. Read-only resolution stays open.
            "agent.suspend" => {
                #[derive(Deserialize)]
                struct SuspendParams {
                    agent_id: String,
                    #[serde(default)]
                    reason: Option<String>,
                    #[serde(default = "default_suspended_by")]
                    suspended_by: String,
                }
                fn default_suspended_by() -> String {
                    "operator".to_string()
                }
                let params: SuspendParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for agent.suspend: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match store.suspend_agent(
                    &params.agent_id,
                    &params.suspended_by,
                    params.reason.as_deref(),
                ) {
                    Ok(changed) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "ok": true,
                            "agent_id": params.agent_id,
                            // false when the agent was already suspended or has
                            // no promoted alias to suspend.
                            "suspended": changed,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("agent.suspend failed: {}", e),
                    ),
                }
            }

            // Lift a suspension. Re-promotion also clears it (unless the
            // promotion was envelope-pre-authorized); this is the explicit lever.
            "agent.unsuspend" => {
                #[derive(Deserialize)]
                struct UnsuspendParams {
                    agent_id: String,
                }
                let params: UnsuspendParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for agent.unsuspend: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "Gateway store not available".to_string(),
                        );
                    }
                };
                match store.unsuspend_agent(&params.agent_id) {
                    Ok(changed) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "ok": true,
                            "agent_id": params.agent_id,
                            // false when the agent was not suspended.
                            "unsuspended": changed,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("agent.unsuspend failed: {}", e),
                    ),
                }
            }

            // Session fork - fork a session from a snapshot
            // List the turns that have a runnable checkpoint — the points a
            // session can be forked from. The room marks these on the timeline
            // so the operator can see where `/fork --at-turn N` (or `F`) will work.
            "session.checkpoints" => {
                #[derive(Deserialize)]
                struct CheckpointsParams {
                    source_session_id: String,
                }
                let params: CheckpointsParams = match serde_json::from_value(req.params.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.checkpoints: {}", e),
                        );
                    }
                };
                let turn_ids = crate::runtime::checkpoint::list_checkpoints(
                    &self.config,
                    &params.source_session_id,
                )
                .unwrap_or_default();
                let turns: Vec<u64> = turn_ids
                    .iter()
                    .filter_map(|id| id.strip_prefix("turn-").and_then(|n| n.parse::<u64>().ok()))
                    .collect();
                return JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({ "turn_ids": turn_ids, "turns": turns }),
                );
            }
            "session.fork" => {
                #[derive(Deserialize)]
                struct ForkParams {
                    source_session_id: String,
                    #[serde(default)]
                    branch_message: Option<String>,
                    #[serde(default)]
                    new_session_id: Option<String>,
                    #[serde(default)]
                    target_agent_id: Option<String>,
                    /// Optional turn number to branch from. When omitted, forks
                    /// from the latest checkpoint. Checkpoints exist only at
                    /// yield points, so not every turn is forkable.
                    #[serde(default)]
                    at_turn: Option<u64>,
                }

                let params: ForkParams = match serde_json::from_value(req.params.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for session.fork: {}", e),
                        );
                    }
                };

                // Fork from a specific historical turn when requested, else
                // from the latest checkpoint of the source session.
                let fork_result = if let Some(turn) = params.at_turn {
                    match crate::runtime::checkpoint::load_checkpoint(
                        &self.config,
                        &params.source_session_id,
                        &crate::runtime::checkpoint::turn_id_for(turn),
                    ) {
                        Ok(Some(checkpoint)) => {
                            crate::runtime::checkpoint::SessionFork::fork_from_checkpoint(
                                &self.config,
                                &checkpoint,
                                params.new_session_id.as_deref(),
                                params.branch_message.as_deref(),
                            )
                        }
                        Ok(None) => {
                            let available = crate::runtime::checkpoint::list_checkpoints(
                                &self.config,
                                &params.source_session_id,
                            )
                            .unwrap_or_default();
                            let hint = if available.is_empty() {
                                " (no checkpoints exist for this session)".to_string()
                            } else {
                                format!(" — forkable turns: {}", available.join(", "))
                            };
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!(
                                    "No checkpoint found for session '{}' at turn {}{}",
                                    params.source_session_id, turn, hint
                                ),
                            );
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    crate::runtime::checkpoint::SessionFork::fork(
                        &self.config,
                        &params.source_session_id,
                        params.new_session_id.as_deref(),
                        params.branch_message.as_deref(),
                    )
                };
                let fork = match fork_result {
                    Ok(f) => f,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to fork session: {}", e),
                        );
                    }
                };

                // Determine the acting agent for lineage/causal attribution:
                // an explicit target_agent_id, else the agent the source
                // checkpoint was running (NOT the source session id — a
                // session id is not an agent).
                let target_agent_id = params
                    .target_agent_id
                    .unwrap_or_else(|| fork.agent_id.clone());

                // Single choke point for every fork side effect (timeline
                // mirror, lineage row, both causal events) — shared with
                // `trace fork` (CLI) so the two paths can't drift (#814).
                // The lineage row is load-bearing (artifact-ref resolution
                // across fork boundaries depends on it), so a failure there
                // is logged loudly; but the fork itself already succeeded on
                // disk, so we still return success with mirrored_events = 0
                // rather than fail an otherwise-complete operation.
                let mirrored_events = match self.execution.gateway_store() {
                    Some(store) => match store.record_session_fork(
                        &fork,
                        params.branch_message.as_deref(),
                        &target_agent_id,
                    ) {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::error!(
                                target: "session.fork",
                                new = %fork.new_session_id,
                                source = %params.source_session_id,
                                error = %e,
                                "Failed to record fork lineage"
                            );
                            0
                        }
                    },
                    None => 0,
                };

                JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "new_session_id": fork.new_session_id,
                        "source_session_id": fork.source_session_id,
                        "fork_turn": fork.fork_turn,
                        "history_handle": fork.history_handle,
                        "message_count": fork.initial_history.len(),
                        "mirrored_events": mirrored_events,
                    }),
                )
            }

            "artifact.list_files" => {
                #[derive(Deserialize)]
                struct ArtifactListParams {
                    artifact_ref: String,
                    #[serde(default)]
                    session_id: String,
                }
                let params: ArtifactListParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for artifact.list_files: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::artifact_store::ArtifactStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("artifact store open failed: {}", e),
                        );
                    }
                };
                let artifact_id = if params.artifact_ref.starts_with("art_") {
                    params.artifact_ref.clone()
                } else {
                    let gw_store = match self.execution.gateway_store() {
                        Some(s) => s,
                        None => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                "Gateway store not available for ref resolution".to_string(),
                            );
                        }
                    };
                    match gw_store.resolve_artifact_ref_any_scope(&params.artifact_ref, &params.session_id) {
                        Ok(Some(rec)) => rec.artifact_id,
                        Ok(None) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("artifact ref '{}' not found or expired", params.artifact_ref),
                            );
                        }
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("artifact ref resolution failed: {}", e),
                            );
                        }
                    }
                };
                match store.inspect(&artifact_id) {
                    Ok(bundle) => {
                        let files: Vec<serde_json::Value> = bundle
                            .files
                            .iter()
                            .map(|f| {
                                serde_json::json!({
                                    "name": f.name,
                                    "handle": f.handle,
                                    "alias": f.alias,
                                })
                            })
                            .collect();
                        let layers: Vec<serde_json::Value> = bundle
                            .layers
                            .iter()
                            .map(|l| {
                                serde_json::json!({
                                    "layer_id": l.layer_id,
                                    "name": l.name,
                                    "mount_path": l.mount_path,
                                    "digest": l.digest,
                                })
                            })
                            .collect();
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "artifact_id": bundle.artifact_id,
                                "artifact_ref": params.artifact_ref,
                                "kind": format!("{:?}", bundle.kind),
                                "files": files,
                                "layers": layers,
                                "created_at": bundle.created_at,
                            }),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("artifact.list_files failed: {}", e),
                    ),
                }
            }

            "artifact.layer_inspect" => {
                #[derive(Deserialize)]
                struct LayerInspectParams {
                    layer_id: String,
                    #[serde(default)]
                    include_files: bool,
                }
                let params: LayerInspectParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for artifact.layer_inspect: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let layer_store = match crate::layer_store::LayerStore::new(
                    &gateway_dir,
                    Default::default(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("layer store open failed: {}", e),
                        );
                    }
                };
                match layer_store.inspect(&params.layer_id) {
                    Ok(manifest) => {
                        // Serialize the manifest fields. `resolved_packages`
                        // and `approval_scope` use skip_serializing_if so they
                        // stay absent for layers built before those existed.
                        let mut payload = serde_json::json!({
                            "layer_id": manifest.layer_id,
                            "name": manifest.name,
                            "digest": manifest.digest,
                            "file_count": manifest.file_count,
                            "size_bytes": manifest.size_bytes,
                            "created_at": manifest.created_at,
                        });
                        if !manifest.resolved_packages.is_empty() {
                            payload["resolved_packages"] = serde_json::to_value(&manifest.resolved_packages)
                                .unwrap_or(serde_json::Value::Null);
                        }
                        if let Some(ref scope) = manifest.approval_scope {
                            payload["approval_scope"] = serde_json::to_value(scope)
                                .unwrap_or(serde_json::Value::Null);
                        }
                        if params.include_files {
                            match layer_store.list_files(&params.layer_id, 500) {
                                Ok((entries, total, truncated)) => {
                                    let files: Vec<serde_json::Value> = entries
                                        .iter()
                                        .map(|e| {
                                            serde_json::json!({
                                                "path": e.path,
                                                "size": e.size,
                                            })
                                        })
                                        .collect();
                                    payload["files"] = serde_json::Value::Array(files);
                                    payload["files_total"] = serde_json::json!(total);
                                    payload["files_truncated"] = serde_json::json!(truncated);
                                }
                                Err(e) => {
                                    payload["files_error"] =
                                        serde_json::Value::String(format!("{}", e));
                                }
                            }
                        }
                        JsonRpcResponse::success(req.id, payload)
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("artifact.layer_inspect failed: {}", e),
                    ),
                }
            }

            "artifact.read_file" => {
                #[derive(Deserialize)]
                struct ArtifactReadParams {
                    artifact_ref: String,
                    file_name: String,
                    #[serde(default)]
                    session_id: String,
                }
                let params: ArtifactReadParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for artifact.read_file: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::artifact_store::ArtifactStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("artifact store open failed: {}", e),
                        );
                    }
                };
                let artifact_id = if params.artifact_ref.starts_with("art_") {
                    params.artifact_ref.clone()
                } else {
                    let gw_store = match self.execution.gateway_store() {
                        Some(s) => s,
                        None => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                "Gateway store not available for ref resolution".to_string(),
                            );
                        }
                    };
                    match gw_store.resolve_artifact_ref_any_scope(&params.artifact_ref, &params.session_id) {
                        Ok(Some(rec)) => rec.artifact_id,
                        Ok(None) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("artifact ref '{}' not found or expired", params.artifact_ref),
                            );
                        }
                        Err(e) => {
                            return JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("artifact ref resolution failed: {}", e),
                            );
                        }
                    }
                };
                match store.inspect(&artifact_id) {
                    Ok(bundle) => {
                        let file_entry = match bundle.files.iter().find(|f| f.name == params.file_name) {
                            Some(f) => f,
                            None => {
                                return JsonRpcResponse::error(
                                    req.id,
                                    -32000,
                                    format!(
                                        "file '{}' not found in artifact '{}'",
                                        params.file_name, artifact_id
                                    ),
                                );
                            }
                        };
                        match store.content_store().read_string(&file_entry.handle) {
                            Ok(content) => JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({
                                    "artifact_id": artifact_id,
                                    "file_name": params.file_name,
                                    "content": content,
                                }),
                            ),
                            Err(e) => JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("artifact.read_file failed: {}", e),
                            ),
                        }
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("artifact.read_file failed: {}", e),
                    ),
                }
            }

            "content.list" => {
                // List every content-store entry (name → handle) for a session.
                // Mirrors `artifact.list_files` but over the live content store,
                // so the operator can see what the session is producing in
                // realtime — before any artifact is built (Pillar D, t=0
                // visibility). Drafts are content-addressed blobs under mutable
                // names; immutability is untouched.
                #[derive(Deserialize)]
                struct ContentListParams {
                    session_id: String,
                }
                let params: ContentListParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for content.list: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::runtime::content_store::ContentStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("content store open failed: {}", e),
                        );
                    }
                };
                let names_handles = match store.list_names_with_handles(&params.session_id) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("content.list failed: {}", e),
                        );
                    }
                };
                // Cross-session visibility: a handle can be registered under
                // a child session but declared global. We need the LOCAL
                // manifest (per-session visibility for this session_id) AND
                // a probe of the GLOBAL manifest (where global entries are
                // registered under the sentinel "__global__" session).
                // The local map is authoritative for private/session; if a
                // handle is missing locally, we fall back to the global
                // manifest so global entries written by child sessions are
                // labelled "global" (not "session") — and the UI shows the
                // 🌐 badge correctly.
                let visibility_map = store
                    .load_manifest(&params.session_id)
                    .map(|m| m.visibility)
                    .unwrap_or_default();
                let global_handles: std::collections::HashSet<String> = store
                    .load_manifest(crate::runtime::content_store::GLOBAL_SESSION_ID)
                    .map(|m| m.names.values().cloned().collect())
                    .unwrap_or_default();
                let files: Vec<serde_json::Value> = names_handles
                    .into_iter()
                    .map(|(name, handle)| {
                        let alias = crate::runtime::content_store::ContentStore::get_short_alias(&handle);
                        let visibility = visibility_map
                            .get(&handle)
                            .map(|v| match v {
                                crate::runtime::content_store::ContentVisibility::Private => "private",
                                crate::runtime::content_store::ContentVisibility::Session => "session",
                                crate::runtime::content_store::ContentVisibility::Global => "global",
                            })
                            .unwrap_or(if global_handles.contains(&handle) { "global" } else { "session" });
                        serde_json::json!({
                            "name": name,
                            "handle": handle,
                            "alias": alias,
                            "visibility": visibility,
                        })
                    })
                    .collect();
                JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "session_id": params.session_id,
                        "files": files,
                    }),
                )
            }

            // Materialize the session's live content drafts into a real
            // directory the operator can open in an external editor. Read-only
            // snapshot, rebuilt on each call; never feeds back into the store
            // (the agent's working state is untouched). Tier 1 of the live
            // workbench (#524).
            "content.project_live" => {
                #[derive(Deserialize)]
                struct ContentProjectLiveParams {
                    session_id: String,
                }
                let params: ContentProjectLiveParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for content.project_live: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::runtime::content_store::ContentStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("content store open failed: {}", e),
                        );
                    }
                };
                match store.project_live(&params.session_id) {
                    Ok((dir, files)) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "ok": true,
                            "session_id": params.session_id,
                            "path": dir.to_string_lossy(),
                            "files": files,
                            "count": files.len(),
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("content.project_live failed: {}", e),
                    ),
                }
            }

            "content.read" => {
                // Read a content-store entry's bytes by name or handle. Mirrors
                // `artifact.read_file` but over the live content store.
                #[derive(Deserialize)]
                struct ContentReadParams {
                    session_id: String,
                    name: String,
                }
                let params: ContentReadParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for content.read: {}", e),
                        );
                    }
                };
                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::runtime::content_store::ContentStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("content store open failed: {}", e),
                        );
                    }
                };
                // Resolve the handle ONCE first, then read by the resolved
                // handle. A read that succeeds against a name/alias must have
                // a real handle behind it; returning an empty string when
                // resolution fails would produce an inconsistent response
                // (bytes present, handle missing). If the name/handle does
                // not resolve at all, fail fast with -32000.
                let handle = match store
                    .resolve_name_or_handle_to_handle(&params.session_id, &params.name)
                {
                    Ok(h) => h,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("content.read resolve failed: {}", e),
                        );
                    }
                };
                match store.read_by_name_or_handle(&params.session_id, &handle) {
                    Ok(bytes) => {
                        // Lossy UTF-8: the viewer is text-oriented (markdown);
                        // binary blobs surface a replacement-char placeholder.
                        let content = String::from_utf8_lossy(&bytes).into_owned();
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "name": params.name,
                                "handle": handle,
                                "bytes": bytes.len(),
                                "content": content,
                            }),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("content.read failed: {}", e),
                    ),
                }
            }

            // Attach an operator comment to a live content file (file-level +
            // optional line hint), record it on the timeline, and deliver it to
            // the owning agent at its next turn. Comment-only — never mutates
            // agent state. See `docs/design/operator-live-comments.md`.
            "content.comment" => {
                #[derive(Deserialize)]
                struct ContentCommentParams {
                    session_id: String,
                    name: String,
                    /// The content version the operator was viewing (anchor).
                    /// Omitted → anchor to the current version.
                    #[serde(default)]
                    handle: Option<String>,
                    #[serde(default)]
                    line_start: Option<u32>,
                    #[serde(default)]
                    line_end: Option<u32>,
                    body: String,
                    #[serde(default = "default_commented_by")]
                    commented_by: String,
                }
                fn default_commented_by() -> String {
                    "operator".to_string()
                }

                let id = req.id.clone();
                let auth_token = req.auth_token.clone();
                let params: ContentCommentParams = match serde_json::from_value(req.params) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32602,
                            format!("Invalid params for content.comment: {}", e),
                        );
                    }
                };

                if params.body.trim().is_empty() {
                    return JsonRpcResponse::error(
                        id,
                        -32602,
                        "content.comment requires a non-empty body".to_string(),
                    );
                }
                if let (Some(s), Some(e)) = (params.line_start, params.line_end) {
                    if e < s {
                        return JsonRpcResponse::error(
                            id,
                            -32602,
                            format!(
                                "content.comment line_end ({e}) precedes line_start ({s})"
                            ),
                        );
                    }
                }

                let gateway_dir = crate::execution::gateway_root_dir(self.config.as_ref());
                let store = match crate::runtime::content_store::ContentStore::new(&gateway_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("content store open failed: {}", e),
                        );
                    }
                };
                // Resolve the current version. A comment must reference a name
                // that actually exists in the session.
                let current_handle = match store
                    .resolve_name_or_handle_to_handle(&params.session_id, &params.name)
                {
                    Ok(h) => h,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            -32000,
                            format!("content.comment resolve failed: {}", e),
                        );
                    }
                };
                // Anchor to the viewed version; flag drift when the file has
                // moved on since then.
                let anchor_handle =
                    params.handle.clone().unwrap_or_else(|| current_handle.clone());
                let drifted = anchor_handle != current_handle;

                let comment_id = autonoetic_types::id_format::short_random_id_hex("cmt_", 12);
                let redacted_body =
                    crate::log_redaction::redact_text_for_logs(&params.body);

                let line_hint = match (params.line_start, params.line_end) {
                    (Some(s), Some(e)) if e != s => format!(", lines {s}–{e}"),
                    (Some(s), _) => format!(", line {s}"),
                    _ => String::new(),
                };

                // Presentation: operator.comment row on the canonical timeline
                // (Attention) so every channel surfaces it.
                if let Some(ts) = self.execution.gateway_store() {
                    let payload = serde_json::json!({
                        "comment_id": comment_id,
                        "name": params.name,
                        "handle": anchor_handle,
                        "current_handle": current_handle,
                        "drifted": drifted,
                        "line_start": params.line_start,
                        "line_end": params.line_end,
                        "body": redacted_body,
                    });
                    let event = crate::runtime::session_timeline::operator_comment_event(
                        &params.session_id,
                        &params.commented_by,
                        payload,
                    );
                    if let Err(e) = ts.create_live_digest_event(&event) {
                        tracing::debug!(
                            target: "session_timeline",
                            error = %e,
                            "operator.comment timeline emit failed"
                        );
                    }
                }

                // Delivery: frame the comment and hand it to the owning agent at
                // its next turn via the existing event.ingest path. A distinct
                // event_type ("operator_comment", not "chat") avoids emitting a
                // duplicate operator.message row.
                let mut framed = format!(
                    "Operator comment on file `{}` (version {}{}):\n> {}",
                    params.name,
                    anchor_handle,
                    line_hint,
                    params.body.trim()
                );
                if drifted {
                    framed.push_str(&format!(
                        "\n[note: this file has changed since the commented version \
                         (current {current_handle}); re-read the current version \
                         before acting on the line numbers.]"
                    ));
                }
                framed.push_str(
                    "\nAcknowledge this operator comment, then either address it \
                     (say how) or explain why not.",
                );

                let ingest_req = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    method: "event.ingest".to_string(),
                    params: serde_json::json!({
                        "event_type": "operator_comment",
                        "message": framed,
                        "session_id": params.session_id,
                        "async_mode": true,
                        "metadata": {
                            "source": "session_room",
                            "kind": "operator_comment",
                            "comment_id": comment_id,
                            "name": params.name,
                            "handle": anchor_handle,
                            "current_handle": current_handle,
                            "drifted": drifted,
                        },
                    }),
                    auth_token,
                };
                let ingest_resp = Box::pin(self.dispatch(ingest_req)).await;
                if let Some(err) = ingest_resp.error {
                    return JsonRpcResponse::error(
                        id,
                        -32000,
                        format!("content.comment delivery failed: {}", err.message),
                    );
                }

                JsonRpcResponse::success(
                    id,
                    serde_json::json!({
                        "ok": true,
                        "comment_id": comment_id,
                        "name": params.name,
                        "handle": anchor_handle,
                        "drifted": drifted,
                    }),
                )
            }

            "gate.get_messages" => {
                #[derive(Deserialize)]
                struct GetMessagesParams {
                    gate_id: String,
                }

                let params: GetMessagesParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for gate.get_messages: {}", e),
                        );
                    }
                };

                match self.execution.gateway_store() {
                    Some(store) => match store.get_gate_messages(&params.gate_id) {
                        Ok(msgs) => JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "gate_id": params.gate_id,
                                "messages": msgs,
                            }),
                        ),
                        Err(e) => JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to get gate messages: {}", e),
                        ),
                    },
                    None => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available",
                    ),
                }
            }

            "gate.add_message" => {
                #[derive(Deserialize)]
                struct AddMessageParams {
                    gate_id: String,
                    sender: String,
                    content: String,
                }

                let params: AddMessageParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for gate.add_message: {}", e),
                        );
                    }
                };

                if params.gate_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "gate_id must not be empty",
                    );
                }
                if params.content.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "content must not be empty",
                    );
                }
                match params.sender.trim() {
                    "operator" | "system" | "agent" => {}
                    other => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("sender must be one of: operator, system, agent (got: {})", other),
                        );
                    }
                }

                match self.execution.gateway_store() {
                    Some(store) => match store.add_gate_message(
                        params.gate_id.trim(),
                        params.sender.trim(),
                        &crate::log_redaction::redact_text_for_logs(&params.content),
                    ) {
                        Ok(id) => JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "message_id": id,
                            }),
                        ),
                        Err(e) => JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to add gate message: {}", e),
                        ),
                    },
                    None => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available",
                    ),
                }
            }

            "approvals.approve" => {
                #[derive(Deserialize)]
                struct ApproveParams {
                    request_id: String,
                    decided_by: String,
                    reason: Option<String>,
                    #[serde(default)]
                    secrets: Option<Vec<(String, String)>>,
                    approver_level: Option<String>,
                    #[serde(default)]
                    confirm_phrase: Option<String>,
                    #[serde(default)]
                    acknowledged_capabilities: Vec<String>,
                    #[serde(default)]
                    decider_session_id: Option<String>,
                    /// When `false`, skip session-grant creation even if the
                    /// action carries `detected_hosts`. This makes the approval
                    /// a one-shot: only this invocation is authorized.
                    #[serde(default)]
                    create_grant: Option<bool>,
                }

                let params: ApproveParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for approvals.approve: {}", e),
                        );
                    }
                };

                if params.request_id.trim().is_empty() || params.decided_by.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "request_id and decided_by must not be empty",
                    );
                }

                let config = self.execution.config();
                let store = self.execution.gateway_store();
                let hooks = self.execution.hook_executor();
                let level = params.approver_level.as_deref().map(|s| match s {
                    "admin" => autonoetic_types::background::ApprovalLevel::Admin,
                    s if s.starts_with("agent:") => {
                        autonoetic_types::background::ApprovalLevel::Agent(
                            s.strip_prefix("agent:").unwrap_or(s).to_string(),
                        )
                    }
                    _ => autonoetic_types::background::ApprovalLevel::Operator,
                });

                match crate::scheduler::approve_request_with_options(
                    config.as_ref(),
                    store.as_deref(),
                    params.request_id.trim(),
                    params.decided_by.trim(),
                    params.reason,
                    params.secrets,
                    level.as_ref(),
                    Some(hooks.as_ref()),
                    crate::scheduler::ApproveOptions {
                        acknowledged_capabilities: params.acknowledged_capabilities,
                        confirm_phrase: params.confirm_phrase,
                        decider_session_id: params.decider_session_id,
                        create_grant: params.create_grant,
                        ..Default::default()
                    },
                ) {
                    Ok(decision) => {
                        self.transition_async_to_processing(
                            &decision.session_id,
                            decision.root_session_id.as_deref(),
                        ).await;

                        // Directly trigger session resume for non-workflow-bound
                        // approvals.  Without this, sessions that ended their
                        // turn (Hibernation / HumanEscalation checkpoint) after
                        // requesting an approval stay stuck until the operator
                        // manually sends a "continue" message — the async
                        // notification pump is best-effort and may lag.
                        if crate::scheduler::approval::should_resume_waiting_session(&decision) {
                            let resume_msg = format!(
                                "approval_resolved:{}:{}",
                                decision.request_id,
                                decision.status.as_str()
                            );
                            let metadata = serde_json::json!({
                                "sender_id": "approval-resume",
                                "signal_delivered": true,
                                "approval_request_id": decision.request_id,
                                "approval_status": decision.status.as_str(),
                            });
                            if let Err(e) = self.spawn_agent_once(
                                &decision.agent_id,
                                &resume_msg,
                                &decision.session_id,
                                None,
                                false,
                                Some("approval_resolved"),
                                Some(&metadata),
                            ).await {
                                tracing::warn!(
                                    target: "router",
                                    request_id = %decision.request_id,
                                    session_id = %decision.session_id,
                                    error = %e,
                                    "Direct session resume after approval failed; \
                                     falling back to async notification pump",
                                );
                            }
                        }

                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "request_id": decision.request_id,
                                "status": format!("{:?}", decision.status),
                            }),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Approval failed: {}", e),
                    ),
                }
            }

            "approvals.inspect" => {
                #[derive(Deserialize)]
                struct InspectParams {
                    request_id: String,
                }
                let params: InspectParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for approvals.inspect: {}", e),
                        );
                    }
                };
                let store = match self.execution.gateway_store() {
                    Some(s) => s,
                    None => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            "GatewayStore not available",
                        );
                    }
                };
                match store.get_approval(params.request_id.trim()) {
                    Ok(Some(approval)) => {
                        use autonoetic_types::background::ScheduledAction;
                        let mut added_capabilities = Vec::new();
                        let mut broadened_capabilities = Vec::new();
                        let mut extra = serde_json::Map::new();
                        match &approval.action {
                            ScheduledAction::RevisionPromote {
                                added_capabilities: added,
                                broadened_capabilities: broadened,
                                agent_id,
                                revision_id,
                                ..
                            } => {
                                added_capabilities = added.clone();
                                broadened_capabilities = broadened.clone();
                                extra.insert("agent_id".into(), serde_json::json!(agent_id));
                                extra.insert("revision_id".into(), serde_json::json!(revision_id));
                            }
                            ScheduledAction::SessionEscalate {
                                session_id,
                                root_session_id,
                                requested_by_agent_id,
                                reason,
                                context,
                                urgency,
                                suggested_actions,
                                ..
                            } => {
                                extra.insert("reason".into(), serde_json::json!(reason));
                                extra.insert("urgency".into(), serde_json::json!(urgency));
                                extra.insert("session_id".into(), serde_json::json!(session_id));
                                extra.insert("root_session_id".into(), serde_json::json!(root_session_id));
                                extra.insert(
                                    "requested_by_agent_id".into(),
                                    serde_json::json!(requested_by_agent_id),
                                );
                                extra.insert("context".into(), serde_json::json!(context));
                                extra.insert(
                                    "suggested_actions".into(),
                                    serde_json::json!(suggested_actions),
                                );
                            }
                            ScheduledAction::CredentialPrompt {
                                service,
                                credential_id,
                                message,
                                secret_fields,
                                ..
                            } => {
                                // Surface the secret-field spec so the TUI can render
                                // an in-modal credential entry flow. The secret values
                                // themselves are never part of this response — the
                                // operator provides them at approval time.
                                extra.insert("service".into(), serde_json::json!(service));
                                extra.insert(
                                    "credential_id".into(),
                                    serde_json::json!(credential_id),
                                );
                                if !message.is_empty() {
                                    extra.insert("message".into(), serde_json::json!(message));
                                }
                                extra.insert(
                                    "secret_fields".into(),
                                    serde_json::json!(secret_fields),
                                );
                            }
                            _ => {}
                        }
                        let mut body = serde_json::json!({
                            "request_id": approval.request_id,
                            "status": approval.status.as_ref().map(|s| s.as_str()),
                            "action": approval.action.kind(),
                            "approval_level": approval.approval_level.to_config(),
                            "confirm_phrase": approval.confirm_phrase,
                            "summary": approval.reason,
                            "risk_summary": approval.risk_summary,
                            "added_capabilities": added_capabilities,
                            "broadened_capabilities": broadened_capabilities,
                        });
                        if let Some(obj) = body.as_object_mut() {
                            for (k, v) in extra {
                                obj.insert(k, v);
                            }
                        }
                        JsonRpcResponse::success(req.id, body)
                    }
                    Ok(None) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Approval not found: {}", params.request_id.trim()),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("approvals.inspect failed: {}", e),
                    ),
                }
            }

            "approvals.reject" => {
                #[derive(Deserialize)]
                struct RejectParams {
                    request_id: String,
                    decided_by: String,
                    reason: Option<String>,
                    #[serde(default)]
                    decider_session_id: Option<String>,
                }

                let params: RejectParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for approvals.reject: {}", e),
                        );
                    }
                };

                if params.request_id.trim().is_empty() || params.decided_by.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "request_id and decided_by must not be empty",
                    );
                }

                let config = self.execution.config();
                let store = self.execution.gateway_store();
                let hooks = self.execution.hook_executor();

                match crate::scheduler::reject_request_with_options(
                    config.as_ref(),
                    store.as_deref(),
                    params.request_id.trim(),
                    params.decided_by.trim(),
                    params.reason,
                    Some(hooks.as_ref()),
                    crate::scheduler::ApproveOptions {
                        decider_session_id: params.decider_session_id,
                        ..Default::default()
                    },
                ) {
                    Ok(decision) => {
                        {
                            let mut map = self.async_results.lock().await;
                            map.remove(&decision.session_id);
                            if let Some(root) = &decision.root_session_id {
                                if root != &decision.session_id {
                                    map.remove(root);
                                }
                            }
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "request_id": decision.request_id,
                                "status": format!("{:?}", decision.status),
                            }),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Rejection failed: {}", e),
                    ),
                }
            }

            "workflow.task.retry" => {
                #[derive(Deserialize)]
                struct RetryTaskParams {
                    task_id: String,
                    workflow_id: Option<String>,
                    #[serde(default)]
                    root_session: Option<String>,
                    #[serde(default)]
                    note: Option<String>,
                }

                let params: RetryTaskParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for workflow.task.retry: {}", e),
                        );
                    }
                };

                if params.task_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "task_id must not be empty",
                    );
                }

                let config = self.execution.config();
                let store = self.execution.gateway_store();

                // Resolve workflow_id with the shared helper so the
                // (explicit workflow_id > root_session > error) ladder and the
                // trim/empty-as-None normalization stay identical to the CLI.
                let wf_id = match crate::scheduler::workflow_store::resolve_workflow_id_for_operator_retry(
                    config.as_ref(),
                    params.workflow_id.as_deref(),
                    params.root_session.as_deref(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("workflow lookup failed: {}", e),
                        );
                    }
                };

                match crate::scheduler::workflow_store::retry_workflow_task(
                    config.as_ref(),
                    store.as_deref(),
                    &wf_id,
                    params.task_id.trim(),
                    params.note.as_deref(),
                ) {
                    Ok(task) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "task_id": task.task_id,
                            "workflow_id": task.workflow_id,
                            "agent_id": task.agent_id,
                            "status": "runnable",
                            "retry_count": task.retry_count,
                            "result_summary": task.result_summary,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("workflow.task.retry failed: {}", e),
                    ),
                }
            }

            "approvals.ask_agent" => {
                #[derive(Deserialize)]
                struct AskAgentParams {
                    request_id: String,
                    question: String,
                }

                let params: AskAgentParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for approvals.ask_agent: {}", e),
                        );
                    }
                };

                if params.request_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "request_id must not be empty",
                    );
                }
                if params.question.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "question must not be empty",
                    );
                }

                match self
                    .execution
                    .spawn_clarification_for_approval(
                        params.request_id.trim(),
                        params.question.trim(),
                    )
                    .await
                {
                    Ok(outcome) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "child_session_id": outcome.child_session_id,
                            "answer": outcome.answer,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Clarification spawn failed: {}", e),
                    ),
                }
            }

            "operator.pending" => {
                #[derive(Deserialize)]
                struct PendingParams {
                    root_session_id: String,
                }
                let params: PendingParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for operator.pending: {}", e),
                        );
                    }
                };
                if params.root_session_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "root_session_id must not be empty",
                    );
                }
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for operator.pending",
                    );
                };
                match crate::runtime::operator_pending::collect_pending_for_root(
                    &store,
                    &params.root_session_id,
                    chrono::Utc::now(),
                ) {
                    Ok(pending) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "count": pending.len(),
                            "pending": pending,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("operator.pending failed: {}", e),
                    ),
                }
            }

            "admin.escalation_list" => {
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for admin.escalation_list",
                    );
                };
                match store.list_pending_escalations() {
                    Ok(escalations) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "escalations": escalations,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to list escalations: {}", e),
                    ),
                }
            }

            "admin.escalation_inspect" => {
                #[derive(Deserialize)]
                struct InspectParams {
                    escalation_id: String,
                }
                let params: InspectParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for admin.escalation_inspect: {}", e),
                        );
                    }
                };
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for admin.escalation_inspect",
                    );
                };
                match store.get_escalation(&params.escalation_id) {
                    Ok(Some(escalation)) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "escalation": escalation,
                        }),
                    ),
                    Ok(None) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Escalation '{}' not found", params.escalation_id),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to inspect escalation: {}", e),
                    ),
                }
            }

            "admin.escalation_resolve" => {
                #[derive(Deserialize)]
                struct ResolveParams {
                    escalation_id: String,
                    decided_by: String,
                    status: String,
                    reason: Option<String>,
                }
                let params: ResolveParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for admin.escalation_resolve: {}", e),
                        );
                    }
                };
                if params.escalation_id.trim().is_empty() || params.decided_by.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "escalation_id and decided_by must not be empty",
                    );
                }
                let store = self.execution.gateway_store();
                let Some(store) = store else {
                    return JsonRpcResponse::error(
                        req.id,
                        -32000,
                        "GatewayStore not available for admin.escalation_resolve",
                    );
                };
                let status = match params.status.as_str() {
                    "approved" => autonoetic_types::escalation::EscalationStatus::Approved,
                    "rejected" => autonoetic_types::escalation::EscalationStatus::Rejected,
                    other => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid status '{}'; expected 'approved' or 'rejected'", other),
                        );
                    }
                };
                let config = self.execution.config();
                let hooks = self.execution.hook_executor();
                match store.resolve_escalation(
                    &params.escalation_id,
                    status,
                    &params.decided_by,
                    params.reason.as_deref(),
                ) {
                    Ok(approval_request_id) => {
                        // Bidirectional resolution (#724): if this escalation is a
                        // projection of an approval row, resolve the approval too.
                        if let Some(request_id) = approval_request_id {
                            let approval_status =
                                if status == autonoetic_types::escalation::EscalationStatus::Approved {
                                    autonoetic_types::background::ApprovalStatus::Approved
                                } else {
                                    autonoetic_types::background::ApprovalStatus::Rejected
                                };
                            let result = if approval_status
                                == autonoetic_types::background::ApprovalStatus::Approved
                            {
                                crate::scheduler::approval::approve_request(
                                    &config,
                                    Some(store.as_ref()),
                                    &request_id,
                                    &params.decided_by,
                                    params.reason.clone(),
                                    None,
                                    None,
                                    Some(hooks.as_ref()),
                                )
                            } else {
                                crate::scheduler::approval::reject_request(
                                    &config,
                                    Some(store.as_ref()),
                                    &request_id,
                                    &params.decided_by,
                                    params.reason.clone(),
                                    Some(hooks.as_ref()),
                                )
                            };
                            if let Err(e) = result {
                                tracing::warn!(
                                    target: "router",
                                    escalation_id = %params.escalation_id,
                                    approval_request_id = %request_id,
                                    error = %e,
                                    "Resolved escalation but failed to resolve linked approval"
                                );
                                // Surface the partial resolution instead of
                                // reporting success — otherwise the escalation
                                // reads as resolved while the linked approval
                                // stays pending, the exact orphaned-row state
                                // #724 removes (Part B review).
                                return JsonRpcResponse::error(
                                    req.id,
                                    -32000,
                                    format!(
                                        "Escalation '{}' was resolved, but resolving the linked \
                                         approval '{}' failed: {}. The approval may still be \
                                         pending — retry resolution.",
                                        params.escalation_id, request_id, e
                                    ),
                                );
                            }
                        }
                        JsonRpcResponse::success(
                            req.id,
                            serde_json::json!({
                                "escalation_id": params.escalation_id,
                                "status": params.status,
                                "decided_by": params.decided_by,
                            }),
                        )
                    }
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Failed to resolve escalation: {}", e),
                    ),
                }
            }

            "admin.escalation_ask_role" => {
                #[derive(Deserialize)]
                struct AskRoleParams {
                    escalation_id: String,
                    role_agent_id: String,
                    question: String,
                }
                let params: AskRoleParams = match serde_json::from_value(req.params) {
                    Ok(p) => p,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Invalid params for admin.escalation_ask_role: {}", e),
                        );
                    }
                };
                if params.escalation_id.trim().is_empty() || params.role_agent_id.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "escalation_id and role_agent_id must not be empty",
                    );
                }
                if params.question.trim().is_empty() {
                    return JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "question must not be empty",
                    );
                }
                match self
                    .execution
                    .spawn_clarification_for_escalation(
                        params.escalation_id.trim(),
                        params.role_agent_id.trim(),
                        params.question.trim(),
                    )
                    .await
                {
                    Ok(outcome) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "child_session_id": outcome.child_session_id,
                            "answer": outcome.answer,
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Clarification spawn failed: {}", e),
                    ),
                }
            }

            _ => JsonRpcResponse::error(req.id, -32601, "Method not found"),
        }
    }

    pub async fn spawn_agent_once(
        &self,
        agent_id: &str,
        message: &str,
        session_id: &str,
        source_agent_id: Option<&str>,
        is_message: bool,
        ingest_event_type: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> anyhow::Result<SpawnResult> {
        // Extract workflow_id and task_id from metadata when this is an approval
        // signal delivery. This enables turn continuation resume after approval.
        let (workflow_id, task_id) = metadata
            .and_then(|m| {
                let approval_id = m.get("approval_request_id")?.as_str()?;
                let store = self.execution.gateway_store()?;
                let approval = store.get_approval(approval_id).ok()??;
                Some((approval.workflow_id, approval.task_id))
            })
            .unwrap_or((None, None));

        self.execution
            .spawn_agent_once(
                agent_id,
                message,
                session_id,
                source_agent_id,
                is_message,
                ingest_event_type,
                metadata,
                workflow_id.as_deref(),
                task_id.as_deref(),
                None,
                &[],
            )
            .await
    }

    fn resolve_ingest_target_agent_id(
        &self,
        session_id: &str,
        requested_target_agent_id: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(explicit_target) = requested_target_agent_id.map(str::trim) {
            anyhow::ensure!(
                !explicit_target.is_empty(),
                "target_agent_id must not be empty when provided"
            );
            return Ok(explicit_target.to_string());
        }

        if let Some(store) = self.execution.gateway_store() {
            if let Ok(Some(binding)) = store.get_session_agent_binding(session_id) {
                return Ok(binding.agent_id);
            }
        }

        anyhow::bail!(
            "event.ingest requires an explicit target_agent_id; no default routing is available"
        );
    }

    #[cfg(test)]
    async fn execute_with_reliability_controls<F, Fut, T>(
        &self,
        agent_id: &str,
        operation: F,
    ) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        self.execution
            .execute_with_reliability_controls(agent_id, operation)
            .await
    }

    #[cfg(test)]
    async fn agent_admission_semaphore(&self, agent_id: &str) -> Arc<tokio::sync::Semaphore> {
        self.execution.agent_admission_semaphore(agent_id).await
    }

    #[cfg(test)]
    fn execution_semaphore(&self) -> Arc<tokio::sync::Semaphore> {
        self.execution.execution_semaphore()
    }
}

#[derive(Debug, Deserialize)]
struct AgentSpawnParams {
    agent_id: String,
    #[serde(deserialize_with = "crate::runtime::tools::deserialize_string_lenient")]
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source_agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CurationRunParams {
    root_session_id: String,
    #[serde(default)]
    focus_notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CrystallizeRunParams {
    root_session_id: String,
    #[serde(default)]
    focus_notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventIngestParams {
    event_type: String,
    #[serde(default)]
    target_agent_id: Option<String>,
    message: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source_agent_id: Option<String>,
    /// When true, return immediately with a session acknowledgment and process
    /// the request in the background. The caller polls `session.status` for the
    /// result. Default: false (blocking).
    #[serde(default)]
    async_mode: bool,
}

/// Task-board status for a delegation whose spawn just returned. A spawn that
/// suspended (approval, user input, or waiting for children) is still in flight,
/// so the delegating parent's task stays `Claimed` — reporting `Completed` would
/// tell a planner reading its task board to consume outputs that don't exist yet
/// (#848). Only a fully-completed spawn maps to `Completed`.
fn delegation_task_status_for_spawn(result: &SpawnResult) -> TaskStatus {
    if result.suspended_for_approval.is_some()
        || result.suspended_for_user_input
        || result.suspended_for_child_wait
    {
        TaskStatus::Claimed
    } else {
        TaskStatus::Completed
    }
}

fn append_delegation_task_entry(
    config: &GatewayConfig,
    source_agent_id: &str,
    creator_id: &str,
    action: &str,
    status: TaskStatus,
    result: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    append_task_board_entry(
        config,
        &TaskBoardEntry {
            task_id: uuid::Uuid::new_v4().to_string(),
            creator_id: creator_id.to_string(),
            title: format!("{action} result from {creator_id}"),
            description: format!("Delegated {action} for source agent '{source_agent_id}'"),
            status,
            assignee_id: Some(source_agent_id.to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            capabilities_required: vec![],
            result,
        },
    )
}

/// Emit an `O-7`-tagged causal event recording the anomaly-flag decider
/// obligation outcome (`decider_obligation.refused` / `…satisfied`), mirroring
/// `scheduler::approval::emit_decider_obligation_event`'s shape so
/// contract-health attributes it consistently to O-7 (which entered the
/// enforcement register with the 2026.07.19 amendment). Best-effort:
/// a store/emit failure must not change the decision outcome.
fn emit_anomaly_decider_obligation_event(
    store: &crate::scheduler::gateway_store::GatewayStore,
    flag: &crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag,
    decided_by: &str,
    status: &str,
    action: &str,
) {
    let now = chrono::Utc::now();
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("aflag-obligation-{}", uuid::Uuid::new_v4()),
        agent_id: flag.reporter_agent_id.clone(),
        session_id: flag.reporter_session_id.clone().unwrap_or_default(),
        turn_id: None,
        event_seq: now.timestamp_millis().max(0) as u64,
        timestamp: now.to_rfc3339(),
        category: "decider_obligation".to_string(),
        action: action.to_string(),
        status: if action == "refused" { "error" } else { "success" }.to_string(),
        enforced_rules: vec!["O-7".to_string()],
        target: Some(flag.flag_id.clone()),
        payload: Some(
            serde_json::json!({
                "flag_id": flag.flag_id,
                "status": status,
                "decided_by": decided_by,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(format!("§O (O-7) decider motivation {action}")),
    };
    let _ = store.create_causal_event(&event);
}

// ---------------------------------------------------------------------------
// Session-scoped egress policy handlers (RFC data-envelopes §5.4).
//
// Deliberately out of line and `#[inline(never)]`: `handle_request`'s match has
// thousands of arms and, in a debug build, one stack frame sized by the union
// of every arm's locals. Adding these bodies inline is measurably enough to
// overflow the stack during server bootstrap, so the param structs and their
// deserialization live here instead.
// ---------------------------------------------------------------------------

fn default_egress_policy_actor() -> String {
    "operator:rpc".to_string()
}

fn invalid_egress_params(id: String, method: &str, e: serde_json::Error) -> JsonRpcResponse {
    JsonRpcResponse::error(id, -32602, format!("Invalid params for {method}: {e}"))
}

#[inline(never)]
fn handle_egress_policy_get(
    execution: &GatewayExecutionService,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    #[derive(Deserialize)]
    struct Params {
        session_id: String,
    }
    let params: Params = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return invalid_egress_params(req.id, "session.egress_policy.get", e),
    };
    match execution.get_session_egress_policy(&params.session_id) {
        Ok(v) => JsonRpcResponse::success(req.id, v),
        Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
    }
}

#[inline(never)]
fn handle_egress_policy_set(
    execution: &GatewayExecutionService,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    #[derive(Deserialize)]
    struct Params {
        session_id: String,
        policy: autonoetic_types::egress::EgressSessionPolicy,
        #[serde(default = "default_egress_policy_actor")]
        set_by: String,
    }
    let params: Params = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return invalid_egress_params(req.id, "session.egress_policy.set", e),
    };
    match execution.set_session_egress_policy(&params.session_id, params.policy, &params.set_by) {
        Ok(v) => JsonRpcResponse::success(req.id, v),
        Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
    }
}

#[inline(never)]
fn handle_egress_policy_clear(
    execution: &GatewayExecutionService,
    req: JsonRpcRequest,
) -> JsonRpcResponse {
    #[derive(Deserialize)]
    struct Params {
        session_id: String,
        #[serde(default = "default_egress_policy_actor")]
        set_by: String,
    }
    let params: Params = match serde_json::from_value(req.params) {
        Ok(p) => p,
        Err(e) => return invalid_egress_params(req.id, "session.egress_policy.clear", e),
    };
    match execution.clear_session_egress_policy(&params.session_id, &params.set_by) {
        Ok(v) => JsonRpcResponse::success(req.id, v),
        Err(e) => JsonRpcResponse::error(req.id, -32000, format!("{}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{inbox_path, task_board_path};
    use autonoetic_types::task_board::TaskBoardEntry;
    use tempfile::TempDir;

    fn test_router() -> (TempDir, JsonRpcRouter) {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let router = JsonRpcRouter::new(
            GatewayConfig {
                agents_dir: temp.path().join("agents"),
                ..GatewayConfig::default()
            },
            None,
        );
        (temp, router)
    }

    fn write_minimal_agent(agents_dir: &std::path::Path, agent_id: &str) {
        let agent_dir = agents_dir.join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("SKILL.md"),
            format!(
                "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"{agent_id}\"\n  name: \"{agent_id}\"\n  description: \"test\"\n---\nbody\n"
            ),
        )
        .unwrap();
    }

    fn write_background_agent(agents_dir: &std::path::Path, agent_id: &str, _signals: bool) {
        let agent_dir = agents_dir.join(agent_id);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("SKILL.md"),
            format!(
                "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"{agent_id}\"\n  name: \"{agent_id}\"\n  description: \"test\"\nbackground:\n  enabled: true\n  interval_secs: 60\n  mode: deterministic\n  wake_predicates:\n    timer: false\n---\nbody\n"
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_dispatch_ping() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "1".to_string(),
            method: "ping".to_string(),
            params: serde_json::json!({}),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.result, Some(serde_json::json!("pong")));
        assert!(resp.error.is_none());
    }

    #[tokio::test]
    async fn test_dispatch_gateway_info() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "1b".to_string(),
            method: "gateway.info".to_string(),
            params: serde_json::json!({}),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        let result = resp.result.expect("gateway.info should return payload");
        let digest = result["constitution_digest"]
            .as_str()
            .expect("constitution_digest should be a string");
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(result["gateway_version"].is_string());
        assert!(result["constitution_version"].is_string());
        assert!(result["constitution_format_version"].is_u64());
    }

    #[tokio::test]
    async fn test_dispatch_constitution_get() {
        let (_temp, router) = test_router();
        // Null params (no body) must default to the lightweight view.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "cg".to_string(),
            method: "constitution.get".to_string(),
            params: serde_json::Value::Null,
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        let result = resp.result.expect("constitution.get should return payload");
        assert_eq!(result["digest"].as_str().map(str::len), Some(64));
        assert!(result["version"].is_string());
        assert!(result["signed"].as_bool().unwrap_or(false));
        assert!(result["text"].is_null(), "default view omits the markdown");
        let clauses = result["clauses"].as_array().expect("clauses array");
        assert!(clauses.len() > 100);
        assert!(clauses
            .iter()
            .any(|c| c["id"] == "P-1.1" && c["binds"] == "agent"));

        // include_text attaches the full markdown.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "cg2".to_string(),
            method: "constitution.get".to_string(),
            params: serde_json::json!({ "include_text": true }),
            auth_token: None,
        };
        let result = router
            .dispatch(req)
            .await
            .result
            .expect("constitution.get should return payload");
        assert!(result["text"].as_str().unwrap_or("").contains("P-1.1"));
    }

    /// A router backed by a real `GatewayStore` (`test_router()` intentionally
    /// passes `None` — most dispatch tests don't need one). Needed here
    /// because `constitution.resolve_proposal` / `.list_pending_proposals`
    /// read and write the store.
    fn test_router_with_store() -> (TempDir, JsonRpcRouter, Arc<crate::scheduler::gateway_store::GatewayStore>) {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&agents_dir.join(".gateway"))
                .expect("store should open"),
        );
        let router = JsonRpcRouter::new(
            GatewayConfig {
                agents_dir,
                ..GatewayConfig::default()
            },
            Some(store.clone()),
        );
        (temp, router, store)
    }

    fn sample_proposal(proposal_id: &str) -> crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal {
        crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal {
            proposal_id: proposal_id.to_string(),
            proposer_agent_id: "auditor.default".to_string(),
            proposer_session_id: None,
            kind: "add_right".to_string(),
            target_id: None,
            proposed_text: Some("Agents may do X".to_string()),
            justification: "closes a gap".to_string(),
            evidence_json: serde_json::json!({}),
            status: "pending".to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            sla_breached_at: None,
        }
    }

    // O-6 (Decider Obligations, §O of docs/constitution/versions/2026.07.08):
    // every Ri-0.8 proposal is owed a recorded decision. Before this RPC
    // existed, `decide_constitutional_proposal` had no caller — a proposal
    // could never leave `pending`. These tests pin the adjudication path.
    #[tokio::test]
    async fn test_dispatch_constitution_resolve_proposal() {
        let (_temp, router, store) = test_router_with_store();
        store
            .insert_constitutional_proposal(&sample_proposal("prop-1"))
            .expect("insert should succeed");

        // Visible via the global pending-proposals list before resolution.
        let list_req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "lp1".to_string(),
            method: "constitution.list_pending_proposals".to_string(),
            params: serde_json::Value::Null,
            auth_token: None,
        };
        let list_result = router
            .dispatch(list_req)
            .await
            .result
            .expect("list should return payload");
        let proposals = list_result["proposals"].as_array().expect("proposals array");
        assert!(proposals.iter().any(|p| p["proposal_id"] == "prop-1"));

        // Resolve it.
        let resolve_req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "rp1".to_string(),
            method: "constitution.resolve_proposal".to_string(),
            params: serde_json::json!({
                "proposal_id": "prop-1",
                "decided_by": "operator",
                "status": "approved",
                "reason": "looks good",
            }),
            auth_token: None,
        };
        let resolve_resp = router.dispatch(resolve_req).await;
        assert!(resolve_resp.error.is_none(), "unexpected error: {:?}", resolve_resp.error);
        let result = resolve_resp.result.expect("resolve should return payload");
        assert_eq!(result["status"], "approved");
        assert_eq!(result["decided_by"], "operator");

        // No longer surfaced as pending.
        let list_req2 = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "lp2".to_string(),
            method: "constitution.list_pending_proposals".to_string(),
            params: serde_json::Value::Null,
            auth_token: None,
        };
        let list_result2 = router.dispatch(list_req2).await.result.unwrap();
        let proposals2 = list_result2["proposals"].as_array().unwrap();
        assert!(!proposals2.iter().any(|p| p["proposal_id"] == "prop-1"));

        // The store carries the full, attributed decision record (Ri-0.11 /
        // O-2 — a decision that can be reattributed later is not attributed
        // at all).
        let stored = store
            .get_constitutional_proposal("prop-1")
            .unwrap()
            .expect("proposal should still exist");
        assert_eq!(stored.status, "approved");
        assert_eq!(stored.decided_by.as_deref(), Some("operator"));
        assert_eq!(stored.decision_reason.as_deref(), Some("looks good"));
        assert!(stored.decided_at.is_some());
    }

    #[tokio::test]
    async fn test_dispatch_constitution_resolve_proposal_rejects_unknown_status() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "bad".to_string(),
            method: "constitution.resolve_proposal".to_string(),
            params: serde_json::json!({
                "proposal_id": "prop-x",
                "decided_by": "operator",
                "status": "maybe",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32602));
    }

    #[tokio::test]
    async fn test_dispatch_constitution_resolve_proposal_not_found() {
        let (_temp, router, _store) = test_router_with_store();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "nf".to_string(),
            method: "constitution.resolve_proposal".to_string(),
            params: serde_json::json!({
                "proposal_id": "does-not-exist",
                "decided_by": "operator",
                "status": "approved",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert!(resp.error.is_some(), "resolving an unknown proposal must error, not succeed");
    }

    fn sample_flag(flag_id: &str) -> crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag {
        crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag {
            flag_id: flag_id.to_string(),
            reporter_agent_id: "witness.default".to_string(),
            reporter_session_id: Some("witness-session".to_string()),
            subject_ref: "sess-target-1".to_string(),
            observation: "tool call bypassed policy check".to_string(),
            evidence_json: serde_json::json!([]),
            severity: "high".to_string(),
            status: "pending".to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            sla_breached_at: None,
        }
    }

    // O-7 (future obligation, issue #770 part C.1): every anomaly flag is
    // owed a recorded decision. These tests pin the adjudication path.
    #[tokio::test]
    async fn test_dispatch_anomaly_resolve_happy_path_with_reason() {
        let (_temp, router, store) = test_router_with_store();
        store
            .insert_anomaly_flag(&sample_flag("aflag-1"))
            .expect("insert should succeed");

        let list_req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "lp1".to_string(),
            method: "anomaly.list_pending".to_string(),
            params: serde_json::Value::Null,
            auth_token: None,
        };
        let list_result = router
            .dispatch(list_req)
            .await
            .result
            .expect("list should return payload");
        let flags = list_result["flags"].as_array().expect("flags array");
        assert!(flags.iter().any(|f| f["flag_id"] == "aflag-1"));

        let resolve_req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "rf1".to_string(),
            method: "anomaly.resolve".to_string(),
            params: serde_json::json!({
                "flag_id": "aflag-1",
                "decided_by": "operator",
                "status": "confirmed",
                "reason": "verified via causal trace",
            }),
            auth_token: None,
        };
        let resolve_resp = router.dispatch(resolve_req).await;
        assert!(resolve_resp.error.is_none(), "unexpected error: {:?}", resolve_resp.error);
        let result = resolve_resp.result.expect("resolve should return payload");
        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["decided_by"], "operator");

        let list_req2 = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "lp2".to_string(),
            method: "anomaly.list_pending".to_string(),
            params: serde_json::Value::Null,
            auth_token: None,
        };
        let list_result2 = router.dispatch(list_req2).await.result.unwrap();
        let flags2 = list_result2["flags"].as_array().unwrap();
        assert!(!flags2.iter().any(|f| f["flag_id"] == "aflag-1"));

        let stored = store
            .get_anomaly_flag("aflag-1")
            .unwrap()
            .expect("flag should still exist");
        assert_eq!(stored.status, "confirmed");
        assert_eq!(stored.decided_by.as_deref(), Some("operator"));
        assert_eq!(stored.decision_reason.as_deref(), Some("verified via causal trace"));
        assert!(stored.decided_at.is_some());

        // O-7 decider-obligation event was emitted, tagged with the future rule id.
        let events = store
            .search_causal_events(None, None, 100)
            .expect("search events");
        let obligation = events
            .iter()
            .find(|e| e.category == "decider_obligation" && e.action == "satisfied")
            .expect("decider_obligation.satisfied event exists");
        assert_eq!(obligation.enforced_rules, vec!["O-7".to_string()]);
    }

    #[tokio::test]
    async fn test_dispatch_anomaly_resolve_rejects_unknown_status() {
        let (_temp, router, store) = test_router_with_store();
        store
            .insert_anomaly_flag(&sample_flag("aflag-bad-status"))
            .expect("insert should succeed");
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "bad".to_string(),
            method: "anomaly.resolve".to_string(),
            params: serde_json::json!({
                "flag_id": "aflag-bad-status",
                "decided_by": "operator",
                "status": "maybe",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32602));
    }

    #[tokio::test]
    async fn test_dispatch_anomaly_resolve_not_found() {
        let (_temp, router, _store) = test_router_with_store();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "nf".to_string(),
            method: "anomaly.resolve".to_string(),
            params: serde_json::json!({
                "flag_id": "does-not-exist",
                "decided_by": "operator",
                "status": "confirmed",
                "reason": "x",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert!(resp.error.is_some(), "resolving an unknown flag must error, not succeed");
    }

    #[tokio::test]
    async fn test_dispatch_anomaly_resolve_terminal_decision_without_reason_rejected() {
        let (_temp, router, store) = test_router_with_store();
        store
            .insert_anomaly_flag(&sample_flag("aflag-no-reason"))
            .expect("insert should succeed");
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "unmotivated".to_string(),
            method: "anomaly.resolve".to_string(),
            params: serde_json::json!({
                "flag_id": "aflag-no-reason",
                "decided_by": "operator",
                "status": "dismissed",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        let err = resp.error.expect("unmotivated terminal decision must be rejected");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("O-7"),
            "error must cite the O-7 decider obligation: {}",
            err.message
        );

        // Row must remain pending — the refusal must not have applied the decision.
        let row = store.get_anomaly_flag("aflag-no-reason").unwrap().unwrap();
        assert_eq!(row.status, "pending");

        // A refused decider-obligation event was emitted.
        let events = store.search_causal_events(None, None, 100).expect("search events");
        let obligation = events
            .iter()
            .find(|e| e.category == "decider_obligation" && e.action == "refused")
            .expect("decider_obligation.refused event exists");
        assert_eq!(obligation.enforced_rules, vec!["O-7".to_string()]);
    }

    #[tokio::test]
    async fn test_dispatch_event_ingest_invalid_params() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "2".to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "webhook",
                "target_agent_id": "agent_a"
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32602));
    }

    #[tokio::test]
    async fn test_dispatch_event_ingest_without_target_fails_validation() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "2b".to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "chat",
                "message": "hello planner",
                "session_id": "sess-default-route"
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        let err = resp
            .error
            .expect("event.ingest should fail on missing target");
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("explicit target_agent_id"));
    }

    #[tokio::test]
    async fn test_dispatch_agent_spawn_unauthorized() {
        let (temp, router) = test_router();
        let agents_dir = temp.path().join("agents");

        // Create source agent without AgentSpawn capability
        let source_dir = agents_dir.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nname: source\ndescription: test\ncapabilities:\n  - type: ReadAccess\n    scopes: ['*']\n---\nbody\n",
        ).unwrap();

        // Create target agent
        let target_dir = agents_dir.join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(
            target_dir.join("SKILL.md"),
            "---\nname: target\ndescription: test\n---\nbody\n",
        )
        .unwrap();

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "3".to_string(),
            method: "agent_spawn".to_string(),
            params: serde_json::json!({
                "agent_id": "target",
                "message": "hello",
                "source_agent_id": "source"
            }),
            auth_token: None,
        };

        let resp = router.dispatch(req).await;
        let err = resp.error.expect("Expected an error");
        assert_eq!(err.code, -32000);
        // The source agent has no AgentSpawn capability, so this should fail.
        // With GatewayStore: "Permission Denied: ... lacks 'AgentSpawn' capability"
        // Without GatewayStore: "GatewayStore is required to load agent 'source'"
        assert!(
            err.message.contains("Permission Denied")
                || err.message.contains("GatewayStore is required"),
            "Unexpected error: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_dispatch_agent_spawn_unknown_agent() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "3".to_string(),
            method: "agent_spawn".to_string(),
            params: serde_json::json!({
                "agent_id": "missing",
                "message": "hello"
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32000));
        let msg = &resp.error.as_ref().expect("error should exist").message;
        assert!(
            msg.contains("not found") || msg.contains("GatewayStore is required"),
            "unexpected error: {msg}"
        );
        // Gateway causal chain is no longer used - events are captured in gateway.db
    }

    #[tokio::test]
    async fn test_dispatch_agent_spawn_accepts_metadata_param() {
        let (_temp, router) = test_router();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "3b".to_string(),
            method: "agent_spawn".to_string(),
            params: serde_json::json!({
                "agent_id": "missing",
                "message": "hello",
                "metadata": {
                    "delegated_role": "researcher",
                    "delegation_reason": "need evidence",
                    "expected_outputs": ["summary.md", "sources.json"]
                }
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32000));
        let msg = &resp.error.as_ref().expect("error should exist").message;
        assert!(
            msg.contains("not found") || msg.contains("GatewayStore is required"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_agent_spawn_enforces_max_children_per_session() {
        // NOTE: Gateway causal chain removed - spawn count enforcement is no longer active
        // Spawn events are now captured in gateway.db causal_events table via SessionTracer
        // This test verifies spawn still works without enforcement
        let (_temp, router) = test_router();
        let agents_dir = _temp.path().join("agents");

        let source_dir = agents_dir.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"source\"\n  name: \"source\"\n  description: \"test\"\ncapabilities:\n  - type: AgentSpawn\n    max_children: 1\n---\nbody\n",
        )
        .unwrap();

        let target_dir = agents_dir.join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(
            target_dir.join("SKILL.md"),
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"target\"\n  name: \"target\"\n  description: \"test\"\n  llm_config:\n    model: \"anthropic.claude-sonnet-4-20250514\"\n    max_tokens: 4096\n---\nbody\n",
        )
        .unwrap();

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "4".to_string(),
            method: "agent_spawn".to_string(),
            params: serde_json::json!({
                "agent_id": "target",
                "message": "hello",
                "session_id": "session-1",
                "source_agent_id": "source"
            }),
            auth_token: None,
        };

        // Should succeed since enforcement is disabled
        let resp = router.dispatch(req).await;
        // Either success or error is acceptable - just don't panic
        let _ = resp;
    }

    #[tokio::test]
    async fn test_dispatch_event_ingest_enforces_max_children_per_session() {
        // NOTE: Gateway causal chain removed - event ingest count enforcement is no longer active
        // Event ingest events are now captured in gateway.db via SessionTracer
        // This test verifies event ingest still works without enforcement
        let (_temp, router) = test_router();
        let agents_dir = _temp.path().join("agents");

        let source_dir = agents_dir.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"source\"\n  name: \"source\"\n  description: \"test\"\ncapabilities:\n  - type: AgentSpawn\n    max_children: 1\n---\nbody\n",
        )
        .unwrap();

        let target_dir = agents_dir.join("target");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(
            target_dir.join("SKILL.md"),
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"target\"\n  name: \"target\"\n  description: \"test\"\n  llm_config:\n    model: \"anthropic.claude-sonnet-4-20250514\"\n    max_tokens: 4096\n---\nbody\n",
        )
        .unwrap();

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "5".to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "webhook",
                "target_agent_id": "target",
                "message": "hello",
                "session_id": "session-2",
                "source_agent_id": "source"
            }),
            auth_token: None,
        };

        // Should succeed since enforcement is disabled
        let resp = router.dispatch(req).await;
        // Either success or error is acceptable - just don't panic
        let _ = resp;
    }

    #[tokio::test]
    async fn test_dispatch_event_ingest_creates_notification_for_signal_enabled_agents() {
        let (temp, router) = test_router();
        let agents_dir = temp.path().join("agents");
        write_background_agent(&agents_dir, "target", true);

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "6".to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "webhook",
                "target_agent_id": "target",
                "message": "deploy",
                "session_id": "session-inbox"
            }),
            auth_token: None,
        };

        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32000));

        // Signal-based wake now uses GatewayStore notifications instead of inbox files
        // The notification is created by the scheduler, not the ingress path
        assert!(
            !inbox_path(router.config.as_ref(), "target").exists(),
            "inbox file should not be created for signal-enabled agents"
        );
    }

    #[tokio::test]
    async fn test_dispatch_event_ingest_no_inbox_for_signal_disabled_agents() {
        let (temp, router) = test_router();
        let agents_dir = temp.path().join("agents");
        write_minimal_agent(&agents_dir, "target-no-bg");
        write_background_agent(&agents_dir, "target-no-signals", false);

        for agent_id in ["target-no-bg", "target-no-signals"] {
            let req = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: agent_id.to_string(),
                method: "event.ingest".to_string(),
                params: serde_json::json!({
                    "event_type": "webhook",
                    "target_agent_id": agent_id,
                    "message": "deploy",
                    "session_id": "session-inbox"
                }),
                auth_token: None,
            };

            let resp = router.dispatch(req).await;
            assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32000));
            assert!(
                !inbox_path(router.config.as_ref(), agent_id).exists(),
                "unexpected inbox signal for {agent_id}"
            );
        }
    }

    #[tokio::test]
    async fn test_dispatch_agent_spawn_failure_writes_task_board_entry_for_source() {
        let (temp, router) = test_router();
        let agents_dir = temp.path().join("agents");

        let source_dir = agents_dir.join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"source\"\n  name: \"source\"\n  description: \"test\"\ncapabilities:\n  - type: AgentSpawn\n    max_children: 3\n---\nbody\n",
        )
        .unwrap();
        write_minimal_agent(&agents_dir, "target");

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "7".to_string(),
            method: "agent_spawn".to_string(),
            params: serde_json::json!({
                "agent_id": "target",
                "message": "hello",
                "session_id": "session-task",
                "source_agent_id": "source"
            }),
            auth_token: None,
        };

        let resp = router.dispatch(req).await;
        assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32000));

        let body = std::fs::read_to_string(task_board_path(router.config.as_ref()))
            .expect("task board should exist");
        let entry: TaskBoardEntry =
            serde_json::from_str(body.lines().next().expect("task board entry should exist"))
                .expect("task board entry should decode");
        assert_eq!(entry.assignee_id.as_deref(), Some("source"));
        assert!(matches!(entry.status, TaskStatus::Failed));
        assert_eq!(entry.creator_id, "target");
    }

    #[tokio::test]
    async fn test_reliability_controls_reject_agent_queue_overflow() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let router = JsonRpcRouter::new(
            GatewayConfig {
                agents_dir: temp.path().join("agents"),
                max_concurrent_spawns: 2,
                max_pending_spawns_per_agent: 2,
                ..GatewayConfig::default()
            },
            None,
        );

        let admission = router.agent_admission_semaphore("agent-a").await;
        let _permit1 = admission
            .clone()
            .acquire_owned()
            .await
            .expect("first permit should be acquired");
        let _permit2 = admission
            .clone()
            .acquire_owned()
            .await
            .expect("second permit should be acquired");

        let third = router
            .execute_with_reliability_controls("agent-a", || async { Ok::<_, anyhow::Error>(()) })
            .await;
        assert!(third.is_err());
        assert!(third
            .err()
            .expect("queue overflow should error")
            .to_string()
            .contains("pending execution queue is full"));
    }

    #[tokio::test]
    async fn test_reliability_controls_reject_global_execution_overflow() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let router = JsonRpcRouter::new(
            GatewayConfig {
                agents_dir: temp.path().join("agents"),
                max_concurrent_spawns: 1,
                max_pending_spawns_per_agent: 2,
                ..GatewayConfig::default()
            },
            None,
        );

        let _permit = router
            .execution_semaphore()
            .acquire_owned()
            .await
            .expect("global execution permit should be acquired");

        let second = router
            .execute_with_reliability_controls("agent-b", || async { Ok::<_, anyhow::Error>(()) })
            .await;
        assert!(second.is_err());
        assert!(second
            .err()
            .expect("global overflow should error")
            .to_string()
            .contains("max concurrent executions reached"));
    }

    #[tokio::test]
    async fn test_signal_delivery_idempotency_deduplicates_by_request_id() {
        let (_temp, router) = test_router();

        let make_signal_ingest = |id: &str, signal_req_id: &str| JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "chat",
                "target_agent_id": "planner.default",
                "message": "approval resolved",
                "session_id": "demo-session-1",
                "metadata": {
                    "sender_id": "gateway-signal-poller",
                    "signal_delivered": true,
                    "approval_request_id": signal_req_id,
                    "approval_status": "approved",
                }
            }),
            auth_token: None,
        };

        // First delivery — should NOT be short-circuited (will fail downstream
        // because there's no real agent, but that proves it passed the guard).
        let resp1 = router
            .dispatch(make_signal_ingest("sig-1", "apr-test-dedup"))
            .await;
        assert!(
            resp1.result.is_none() || {
                let r = resp1.result.as_ref().unwrap();
                r.get("status").and_then(|s| s.as_str()) != Some("already_processed")
            },
            "first delivery must not be short-circuited as already_processed"
        );

        // Second delivery with the SAME approval_request_id — must return
        // the idempotent no-op response.
        let resp2 = router
            .dispatch(make_signal_ingest("sig-2", "apr-test-dedup"))
            .await;
        let result2 = resp2
            .result
            .expect("duplicate signal should return success");
        assert_eq!(
            result2.get("status").and_then(|s| s.as_str()),
            Some("already_processed"),
            "second delivery must be deduplicated"
        );
        assert_eq!(
            result2
                .get("signal_request_id")
                .and_then(|s| s.as_str()),
            Some("apr-test-dedup")
        );

        // A DIFFERENT signal_request_id should NOT be deduplicated.
        let resp3 = router
            .dispatch(make_signal_ingest("sig-3", "apr-other"))
            .await;
        let is_dedup = resp3
            .result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            == Some("already_processed");
        assert!(
            !is_dedup,
            "different signal_request_id must not be deduplicated"
        );
    }

    #[tokio::test]
    async fn test_non_signal_ingest_bypasses_idempotency_guard() {
        let (_temp, router) = test_router();

        // A normal event.ingest without signal_delivered metadata should not
        // interact with the idempotency guard at all.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: "normal-1".to_string(),
            method: "event.ingest".to_string(),
            params: serde_json::json!({
                "event_type": "chat",
                "target_agent_id": "planner.default",
                "message": "user typed something",
                "session_id": "demo-session-1",
            }),
            auth_token: None,
        };
        let resp = router.dispatch(req).await;
        let is_dedup = resp
            .result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            == Some("already_processed");
        assert!(!is_dedup, "normal ingest must never hit the idempotency guard");
    }

    #[test]
    fn error_with_rules_populates_data_for_clients() {
        let resp = JsonRpcResponse::error_with_rules(
            "1".into(),
            -32000,
            "agent.spawn failed: promotion incomplete",
            vec!["P-2.25".into(), "P-2.8".into()],
        );
        let err = resp.error.expect("error should be set");
        let rules = err
            .data
            .as_ref()
            .and_then(|d| d.get("enforced_rules"))
            .and_then(|r| r.as_array())
            .expect("data.enforced_rules must be present");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], "P-2.25");
        assert_eq!(rules[1], "P-2.8");
    }

    #[test]
    fn error_with_rules_omits_data_when_no_rules() {
        // No constitutional clause attributed ⇒ data stays None (back-compat
        // with plain `error()`), so clients don't see an empty `enforced_rules`.
        let resp = JsonRpcResponse::error_with_rules("1".into(), -32000, "boom", vec![]);
        let err = resp.error.expect("error should be set");
        assert!(err.data.is_none(), "no rules ⇒ no data envelope");
    }

    #[test]
    fn async_ingest_result_surfaces_enforced_rules_when_failed() {
        // A failed async ingest carries the attributed clause so a client
        // polling `session.status` learns the cause — parity with the sync
        // path's `error.data.enforced_rules`. Empty ⇒ field omitted.
        let mut r = AsyncIngestResult {
            session_id: "s1".into(),
            status: AsyncIngestStatus::Failed,
            assistant_reply: None,
            workflow_note: None,
            artifacts: Vec::new(),
            shared_knowledge: Vec::new(),
            error: Some("promotion incomplete".into()),
            enforced_rules: vec!["P-2.25".into()],
            started_at: "t0".into(),
            completed_at: Some("t1".into()),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["enforced_rules"][0], "P-2.25");

        r.enforced_rules.clear();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("enforced_rules").is_none(), "empty ⇒ omitted");
    }

    /// A `SpawnResult` with the given suspension flags and everything else empty.
    fn spawn_result(
        approval: Option<&str>,
        user_input: bool,
        child_wait: bool,
    ) -> crate::execution::SpawnResult {
        crate::execution::SpawnResult {
            agent_id: "coder.default".into(),
            session_id: "s1".into(),
            assistant_reply: None,
            workflow_note: None,
            should_signal_background: false,
            artifacts: Vec::new(),
            files: Vec::new(),
            shared_knowledge: Vec::new(),
            llm_usage: Vec::new(),
            suspended_for_approval: approval.map(str::to_string),
            suspended_for_user_input: user_input,
            suspended_for_child_wait: child_wait,
        }
    }

    fn async_entry() -> AsyncIngestResult {
        AsyncIngestResult {
            session_id: "s1".into(),
            status: AsyncIngestStatus::Processing,
            assistant_reply: None,
            workflow_note: None,
            artifacts: Vec::new(),
            shared_knowledge: Vec::new(),
            error: None,
            enforced_rules: Vec::new(),
            started_at: "t0".into(),
            completed_at: None,
        }
    }

    #[test]
    fn delegation_status_stays_claimed_while_a_spawn_is_suspended() {
        // #848 site 4: a suspended spawn is still in flight, so the delegating
        // parent's task-board entry must not read Completed — else a planner
        // consumes outputs that don't exist yet.
        // TaskStatus has no PartialEq, so match on the variant.
        assert!(
            matches!(
                delegation_task_status_for_spawn(&spawn_result(None, false, true)),
                TaskStatus::Claimed
            ),
            "child-wait suspension → Claimed"
        );
        assert!(
            matches!(
                delegation_task_status_for_spawn(&spawn_result(Some("apr-1"), false, false)),
                TaskStatus::Claimed
            ),
            "approval suspension → Claimed"
        );
        assert!(
            matches!(
                delegation_task_status_for_spawn(&spawn_result(None, true, false)),
                TaskStatus::Claimed
            ),
            "user-input suspension → Claimed"
        );
        assert!(
            matches!(
                delegation_task_status_for_spawn(&spawn_result(None, false, false)),
                TaskStatus::Completed
            ),
            "no suspension → Completed"
        );
    }

    #[test]
    fn async_entry_status_reflects_child_wait_suspension() {
        // #848 site 1: a child-wait-suspended spawn must surface as
        // SuspendedChildWait with completed_at unset, so an SSE/status client
        // keeps polling instead of concluding the agent is done.
        let mut e = async_entry();
        JsonRpcRouter::apply_spawn_result_to_async_entry(&mut e, &spawn_result(None, false, true));
        assert_eq!(e.status, AsyncIngestStatus::SuspendedChildWait);
        assert!(e.completed_at.is_none(), "child-wait is non-terminal → no completed_at");

        // Contrast: a fully-completed spawn is terminal and stamps completed_at.
        let mut done = async_entry();
        JsonRpcRouter::apply_spawn_result_to_async_entry(
            &mut done,
            &spawn_result(None, false, false),
        );
        assert_eq!(done.status, AsyncIngestStatus::Completed);
        assert!(done.completed_at.is_some(), "completed spawn stamps completed_at");

        // Approval / user-input suspensions map to their own statuses and, like
        // completion, are terminal for the status entry (completed_at set).
        let mut appr = async_entry();
        JsonRpcRouter::apply_spawn_result_to_async_entry(
            &mut appr,
            &spawn_result(Some("apr-1"), false, false),
        );
        assert_eq!(appr.status, AsyncIngestStatus::SuspendedApproval);
        assert!(appr.completed_at.is_some());
    }
}
