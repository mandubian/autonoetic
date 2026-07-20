use crate::llm::ToolDefinition;
use crate::log_redaction::looks_like_secret_collection_prompt;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{default_true, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::UserInteractionStatus;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

fn asks_for_secret(args: &Args) -> bool {
    if looks_like_secret_collection_prompt(&args.question) {
        return true;
    }
    if let Some(ctx) = args.context.as_deref() {
        if looks_like_secret_collection_prompt(ctx) {
            return true;
        }
    }
    args.options.iter().any(|v| {
        ["label", "value", "id"].iter().any(|k| {
            v.get(*k)
                .and_then(|x| x.as_str())
                .map(looks_like_secret_collection_prompt)
                .unwrap_or(false)
        })
    })
}

#[derive(Deserialize)]
struct Args {
    #[serde(default = "default_kind")]
    kind: String,
    question: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    options: Vec<serde_json::Value>,
    #[serde(default = "default_true")]
    allow_freeform: bool,
}

fn default_kind() -> String {
    "clarification".to_string()
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(UserAskTool));
    registry.register(Box::new(UserInteractionStatusTool));
}

pub struct UserAskTool;

impl NativeTool for UserAskTool {
    fn name(&self) -> &'static str {
        "user_ask"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Ask the user a question. Execution suspends until the user answers. Use this for clarifications, decisions, proposals, and confirmations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["clarification", "decision", "proposal", "confirmation"],
                        "default": "clarification",
                        "description": "Type of question being asked"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional context explaining why this question matters"
                    },
                    "options": {
                        "type": "array",
                        "description": "Optional structured choices for the user",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "label": { "type": "string" },
                                "value": { "type": "string" }
                            },
                            "required": ["id", "label", "value"]
                        }
                    },
                    "allow_freeform": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether free text answers are allowed"
                    }
                },
                "required": ["question"],
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
        session_id: Option<&str>,
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        use autonoetic_types::background::UserInteractionOption;
        use crate::runtime::human_gate::{GateKind, GateRequest, GateResult, GateService};

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        if asks_for_secret(&args) {
            return Ok(ToolError::validation(
                "user.ask cannot be used to request secrets or credential values.",
                Some("Use credential.setup / credential.prompt flows so secrets stay in gateway vault-backed channels."),
            )
            .with_code("secret_collection_not_allowed")
            .to_error_response());
        }

        let sid = session_id.unwrap_or("unknown");
        let root_session_id = crate::runtime::content_store::root_session_id(sid).to_string();

        if let (Some(cfg), Some(store)) = (_config, &gateway_store) {
            let workflow_id =
                crate::scheduler::resolve_workflow_id_for_root_session(cfg, &root_session_id)
                    .ok()
                    .flatten();
            if let Some(wf_id) = &workflow_id {
                let task_runs = crate::scheduler::workflow_store::list_task_runs_for_workflow(
                    cfg,
                    Some(store.as_ref()),
                    wf_id,
                )
                .unwrap_or_default();
                let has_active_children = task_runs.iter().any(|t| {
                    if let Some(sid) = session_id {
                        if t.session_id == sid {
                            return false;
                        }
                    }
                    matches!(
                        t.status,
                        autonoetic_types::workflow::TaskRunStatus::Pending
                            | autonoetic_types::workflow::TaskRunStatus::Runnable
                            | autonoetic_types::workflow::TaskRunStatus::Running
                            | autonoetic_types::workflow::TaskRunStatus::AwaitingApproval
                            | autonoetic_types::workflow::TaskRunStatus::Paused
                    )
                });
                if has_active_children {
                    return Ok(ToolError::conflict(
                        "user.ask is not available while workflow tasks are active. Complete or cancel child tasks first.",
                        Some("Call workflow_wait until child tasks complete, then retry."),
                    )
                    .with_code("workflow_tasks_active")
                    .to_error_response());
                }
            }

            let pending_approvals = store
                .get_pending_approvals_for_root(&root_session_id)
                .unwrap_or_default();
            let pending_interactions = store
                .get_pending_interactions_for_root_session(&root_session_id)
                .unwrap_or_default();
            if !pending_approvals.is_empty() || !pending_interactions.is_empty() {
                return Ok(ToolError::conflict(
                    "user.ask is not available while gates are pending. Resolve or wait for pending gates, then retry.",
                    Some("Resolve or wait for pending gates, then retry."),
                )
                .with_code("gates_pending")
                .to_error_response());
            }
        }

        let kind_str = match args.kind.as_str() {
            "decision" => "decision",
            "proposal" => "proposal",
            "confirmation" => "confirmation",
            _ => "clarification",
        };

        let options: Vec<UserInteractionOption> = args
            .options
            .into_iter()
            .filter_map(|v| {
                Some(UserInteractionOption {
                    id: v.get("id")?.as_str()?.to_string(),
                    label: v.get("label")?.as_str()?.to_string(),
                    value: v.get("value")?.as_str()?.to_string(),
                })
            })
            .collect();

        let question_for_side_effects = args.question.clone();
        let opts_summary = if options.is_empty() {
            None
        } else {
            Some(
                options
                    .iter()
                    .map(|o| format!("{}: {}", o.id, o.label))
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        };
        let store = match gateway_store {
            Some(ref s) => s.clone(),
            None => {
                return Ok(ToolError::resource(
                    "Gateway store not available; user.ask requires persistent store",
                    Some("Configure GatewayStore with a persistent backend for user interaction support."),
                )
                .with_code("user_interaction_store_unavailable")
                .to_error_response());
            }
        };

        let gate = GateService::new(store.clone());
        let gate_result = gate.check(GateRequest {
            kind: GateKind::UserInput {
                question: args.question,
                kind: args.kind,
                options: if options.is_empty() {
                    None
                } else {
                    Some(options)
                },
                allow_freeform: args.allow_freeform,
                context: args.context,
            },
            manifest: _manifest,
            session_id: Some(sid),
            run_context: _run_context,
            config: _config,
            context: crate::runtime::human_gate::DecisionContext::tier2(
                format!("agent asks: {}", question_for_side_effects),
                "agent requested operator input",
                "the answer feeds the agent's next step",
                "answer per the question",
            ),
            summary: "user question".to_string(),
            approval_ref: None,
            request_id: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id,
        })?;

        match gate_result {
            GateResult::AlreadyPending { gate_id, .. } => {
                let err = ToolError::conflict(
                    "A user interaction is already pending for this session.",
                    Some("Wait for the existing interaction to be answered, then retry."),
                )
                .with_code("interaction_already_pending");
                let mut v = serde_json::to_value(&err)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                v["interaction_id"] = serde_json::json!(gate_id);
                Ok(v.to_string())
            }
            GateResult::Suspended {
                gate_id,
                response_json,
                ..
            } => {
                if let Some(ctx) = _run_context {
                    if let Some(w) = &ctx.live_digest {
                        if let Ok(mut g) = w.lock() {
                            let _ = g.record_user_ask_pending(
                                &question_for_side_effects,
                                opts_summary.as_deref(),
                            );
                        }
                    }
                    if let Some(w) = &ctx.live_report {
                        if let Ok(mut g) = w.lock() {
                            let _ = g.record_interaction_pending(
                                &gate_id,
                                kind_str,
                                &question_for_side_effects,
                            );
                        }
                    }
                }
                Ok(response_json)
            }
            _ => Ok(serde_json::json!({
                "ok": false,
                "error_type": "unexpected",
                "message": "Unexpected gate result for UserInput",
            })
            .to_string()),
        }
    }
}

pub struct UserInteractionStatusTool;

impl NativeTool for UserInteractionStatusTool {
    fn name(&self) -> &'static str {
        "user_interaction_status"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Query the status of a user interaction. Returns the current status (pending, answered, cancelled, expired) and the answer if available.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "interaction_id": {
                        "type": "string",
                        "description": "The interaction ID to check (e.g., 'ui-abc123')"
                    }
                },
                "required": ["interaction_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            interaction_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "interaction_id": args.interaction_id,
                "status": "unknown",
                "message": "Gateway store not available"
            }))?);
        };

        match store.get_user_interaction(&args.interaction_id) {
            Ok(Some(interaction)) => {
                let same_agent = interaction.agent_id == manifest.agent.id;
                let same_root_scope = session_id
                    .map(crate::runtime::content_store::root_session_id)
                    .map(|root| root == interaction.root_session_id)
                    .unwrap_or(false);
                if !(same_agent || same_root_scope) {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "permission",
                        "interaction_id": args.interaction_id,
                        "message": "Access denied for user interaction status: interaction belongs to a different root session and agent.",
                        "repair_hint": "Query status from the interaction owner agent session or from another session under the same root_session_id.",
                    })
                    .to_string());
                }

                let status = match &interaction.status {
                    UserInteractionStatus::Pending => "pending",
                    UserInteractionStatus::Answered => "answered",
                    UserInteractionStatus::Cancelled => "cancelled",
                    UserInteractionStatus::Expired => "expired",
                };

                let mut response = serde_json::json!({
                    "ok": true,
                    "interaction_id": args.interaction_id,
                    "status": status,
                    "kind": interaction.kind.as_str(),
                    "question": interaction.question,
                    "agent_id": interaction.agent_id,
                    "session_id": interaction.session_id,
                    "created_at": interaction.created_at,
                });

                if let Some(answered_at) = &interaction.answered_at {
                    response["answered_at"] = serde_json::Value::String(answered_at.clone());
                }
                if let Some(answer_text) = &interaction.answer_text {
                    response["answer_text"] = serde_json::Value::String(
                        crate::log_redaction::redact_text_for_logs(answer_text),
                    );
                }
                if let Some(answer_option_id) = &interaction.answer_option_id {
                    response["answer_option_id"] =
                        serde_json::Value::String(answer_option_id.clone());
                }

                serde_json::to_string(&response).map_err(Into::into)
            }
            Ok(None) => serde_json::to_string(&serde_json::json!({
                "ok": true,
                "interaction_id": args.interaction_id,
                "status": "not_found",
                "message": "User interaction not found"
            }))
            .map_err(Into::into),
            Err(e) => Ok(ToolError::resource(
                e.to_string(),
                Some("Verify the interaction id and gateway store availability, then retry."),
            )
            .with_code("interaction_read_failed")
            .to_error_response()),
        }
    }
}
