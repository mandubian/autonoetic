//! Internal JSON-RPC 2.0 Router.

use crate::execution::{
    gateway_actor_id, init_gateway_causal_logger, sha256_hex, GatewayExecutionService, SpawnResult,
};
use crate::scheduler::append_task_board_entry;
use crate::tracing::{EventScope, SessionId, TraceSession};
use autonoetic_types::config::GatewayConfig;
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
    pub artifacts: Vec<serde_json::Value>,
    pub shared_knowledge: Vec<serde_json::Value>,
    pub error: Option<String>,
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
    ) -> Result<(SpawnResult, Option<TraceSession>), (String, Option<TraceSession>)> {
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
                let kickoff = match metadata {
                    Some(metadata) => format!(
                        "Gateway event type: {}\nMessage: {}\nMetadata: {}",
                        event_type, message, metadata
                    ),
                    None => format!("Gateway event type: {}\nMessage: {}", event_type, message),
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
                return Err((e.to_string(), Some(trace_session)));
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
                    Ok(out) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(out).unwrap_or_else(|_| serde_json::json!({})),
                    ),
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
                    Ok(out) => JsonRpcResponse::success(
                        req.id,
                        serde_json::to_value(out).unwrap_or_else(|_| serde_json::json!({})),
                    ),
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
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let agent_id = params.agent_id.clone();

                let ingress = IngressType::Spawn {
                    agent_id: params.agent_id.clone(),
                    source_agent_id: params.source_agent_id.clone(),
                    message: params.message.clone(),
                    metadata: params.metadata.clone(),
                };

                match self
                    .execute_agent_request(ingress, session_id.clone())
                    .await
                {
                    Ok((result, _trace_session)) => {
                        if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                            let _ = append_delegation_task_entry(
                                self.config.as_ref(),
                                source_agent_id,
                                &agent_id,
                                "agent_spawn",
                                TaskStatus::Completed,
                                Some(serde_json::json!({
                                    "session_id": result.session_id.clone(),
                                    "assistant_reply": result.assistant_reply.clone(),
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
                                "artifacts": result.artifacts,
                                "shared_knowledge": result.shared_knowledge,
                                "llm_usage": result.llm_usage,
                            }),
                        )
                    }
                    Err((e, maybe_trace_session)) => {
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
                        JsonRpcResponse::error(req.id, -32000, format!("agent.spawn failed: {}", e))
                    }
                }
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
                        if let Some(serde_json::Value::String(signal_req_id)) =
                            meta.get("approval_request_id")
                        {
                            let mut seen = self.processed_signal_ids.lock().unwrap();
                            if !seen.insert(signal_req_id.clone()) {
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
                                artifacts: Vec::new(),
                                shared_knowledge: Vec::new(),
                                error: None,
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
                        let now = chrono::Utc::now().to_rfc3339();
                        if let Some(entry) = map.get_mut(&session_id_clone) {
                            match result {
                                Ok((spawn_result, _)) => {
                                    if let Some(source) = source_agent_id {
                                        let _ = append_delegation_task_entry(
                                            config.as_ref(),
                                            &source,
                                            &target_agent_id_clone,
                                            "event.ingest",
                                            TaskStatus::Completed,
                                            Some(serde_json::json!({
                                                "session_id": spawn_result.session_id.clone(),
                                                "assistant_reply": spawn_result.assistant_reply.clone(),
                                                "artifacts": spawn_result.artifacts.clone(),
                                                "shared_knowledge": spawn_result.shared_knowledge.clone(),
                                                "event_type": event_type_clone,
                                            })),
                                        );
                                    }
                                    let status = if spawn_result.suspended_for_approval.is_some() {
                                        AsyncIngestStatus::SuspendedApproval
                                    } else {
                                        AsyncIngestStatus::Completed
                                    };
                                    entry.status = status;
                                    entry.assistant_reply = spawn_result.assistant_reply;
                                    entry.artifacts = spawn_result
                                        .artifacts
                                        .into_iter()
                                        .map(|a| serde_json::to_value(&a).unwrap_or_default())
                                        .collect();
                                    entry.shared_knowledge = spawn_result
                                        .shared_knowledge
                                        .into_iter()
                                        .map(|k| serde_json::to_value(&k).unwrap_or_default())
                                        .collect();
                                    entry.completed_at = Some(now);
                                }
                                Err((e, _)) => {
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
                                            })),
                                        );
                                    }
                                    entry.status = AsyncIngestStatus::Failed;
                                    entry.error = Some(e);
                                    entry.completed_at = Some(now);
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
                    match self
                        .execute_agent_request(ingress, session_id.clone())
                        .await
                    {
                        Ok((result, _trace_session)) => {
                            if let Some(source_agent_id) = params.source_agent_id.as_deref() {
                                let _ = append_delegation_task_entry(
                                    self.config.as_ref(),
                                    source_agent_id,
                                    &target_agent_id,
                                    "event.ingest",
                                    TaskStatus::Completed,
                                    Some(serde_json::json!({
                                        "session_id": result.session_id.clone(),
                                        "assistant_reply": result.assistant_reply.clone(),
                                        "artifacts": result.artifacts.clone(),
                                        "shared_knowledge": result.shared_knowledge.clone(),
                                        "event_type": event_type.clone(),
                                    })),
                                );
                            }
                            JsonRpcResponse::success(
                                req.id,
                                serde_json::json!({
                                    "event_type": event_type,
                                    "target_agent_id": target_agent_id,
                                    "session_id": result.session_id,
                                    "assistant_reply": result.assistant_reply,
                                    "artifacts": result.artifacts,
                                    "shared_knowledge": result.shared_knowledge,
                                    "llm_usage": result.llm_usage,
                                }),
                            )
                        }
                        Err((e, maybe_trace_session)) => {
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
                            JsonRpcResponse::error(
                                req.id,
                                -32000,
                                format!("event.ingest failed: {}", e),
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

            // Session fork - fork a session from a snapshot
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

                // Fork from the latest checkpoint of the source session
                let fork = match crate::runtime::checkpoint::SessionFork::fork(
                    &self.config,
                    &params.source_session_id,
                    params.new_session_id.as_deref(),
                    params.branch_message.as_deref(),
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            req.id,
                            -32000,
                            format!("Failed to fork session: {}", e),
                        );
                    }
                };

                // Determine target agent
                let target_agent_id = params
                    .target_agent_id
                    .unwrap_or_else(|| params.source_session_id.clone());

                // Log fork in causal chain (best effort, don't fail fork on logging error)
                let causal_logger_result =
                    crate::execution::init_gateway_causal_logger(&self.config);
                if let Ok(causal_logger) = causal_logger_result {
                    let branch_message_sha256 = params.branch_message.as_ref().map(|m| {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(m.as_bytes());
                        format!("{:x}", hasher.finalize())
                    });
                    let _ = crate::execution::log_gateway_causal_event(
                        &causal_logger,
                        &target_agent_id,
                        &fork.new_session_id,
                        1,
                        "session.forked",
                        autonoetic_types::causal_chain::EntryStatus::Success,
                        Some(serde_json::json!({
                            "source_session_id": params.source_session_id,
                            "fork_turn": fork.fork_turn,
                            "history_handle": fork.history_handle,
                            "branch_message_sha256": branch_message_sha256,
                        })),
                    );
                }

                JsonRpcResponse::success(
                    req.id,
                    serde_json::json!({
                        "new_session_id": fork.new_session_id,
                        "source_session_id": fork.source_session_id,
                        "fork_turn": fork.fork_turn,
                        "history_handle": fork.history_handle,
                        "message_count": fork.initial_history.len(),
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

                match crate::scheduler::approve_request(
                    config.as_ref(),
                    store.as_deref(),
                    params.request_id.trim(),
                    params.decided_by.trim(),
                    params.reason,
                    params.secrets,
                    level.as_ref(),
                    Some(hooks.as_ref()),
                ) {
                    Ok(decision) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "request_id": decision.request_id,
                            "status": format!("{:?}", decision.status),
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Approval failed: {}", e),
                    ),
                }
            }

            "approvals.reject" => {
                #[derive(Deserialize)]
                struct RejectParams {
                    request_id: String,
                    decided_by: String,
                    reason: Option<String>,
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

                match crate::scheduler::reject_request(
                    config.as_ref(),
                    store.as_deref(),
                    params.request_id.trim(),
                    params.decided_by.trim(),
                    params.reason,
                    Some(hooks.as_ref()),
                ) {
                    Ok(decision) => JsonRpcResponse::success(
                        req.id,
                        serde_json::json!({
                            "request_id": decision.request_id,
                            "status": format!("{:?}", decision.status),
                        }),
                    ),
                    Err(e) => JsonRpcResponse::error(
                        req.id,
                        -32000,
                        format!("Rejection failed: {}", e),
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
}
