use crate::llm::ToolDefinition;
use crate::log_redaction::looks_like_secret_collection_prompt;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{default_true, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::UserInteractionStatus;
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
        "user.ask"
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
        use autonoetic_types::background::{
            UserInteraction, UserInteractionKind, UserInteractionOption,
        };

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        if asks_for_secret(&args) {
            return Ok(serde_json::json!({
                "ok": false,
                "error_type": "validation",
                "message": "user.ask cannot be used to request secrets or credential values.",
                "repair_hint": "Use credential.setup / credential.prompt flows so secrets stay in gateway vault-backed channels.",
                "error": "secret_collection_not_allowed"
            }).to_string());
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
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "conflict",
                        "message": "user.ask is not available while workflow tasks are active. Use workflow.wait to handle pending child tasks, or respond in prose for clarifications.",
                        "repair_hint": "Call workflow.wait until child tasks complete, then retry user.ask.",
                        "error": "user.ask is not available while workflow tasks are active. Use workflow.wait to handle pending child tasks, or respond in prose for clarifications."
                    }).to_string());
                }
            }

            let pending_approvals = store
                .get_pending_approvals_for_root(&root_session_id)
                .unwrap_or_default();
            let session_blocking_approvals: Vec<_> = pending_approvals
                .iter()
                .filter(|r| {
                    !matches!(
                        r.action,
                        autonoetic_types::background::ScheduledAction::SandboxExec { .. }
                    )
                })
                .collect();
            if !session_blocking_approvals.is_empty() {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "conflict",
                    "message": "user.ask is not available while approvals are pending. Use workflow.wait to handle pending approvals.",
                    "repair_hint": "Resolve or wait for pending approvals, then retry user.ask.",
                    "error": "user.ask is not available while approvals are pending. Use workflow.wait to handle pending approvals."
                }).to_string());
            }
        }

        let interaction_id = format!("ui-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        let kind = match args.kind.as_str() {
            "decision" => UserInteractionKind::Decision,
            "proposal" => UserInteractionKind::Proposal,
            "confirmation" => UserInteractionKind::Confirmation,
            _ => UserInteractionKind::Clarification,
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

        let now = chrono::Utc::now().to_rfc3339();

        let (workflow_id, task_id, checkpoint_turn_id) = match _run_context {
            Some(ctx) => (
                ctx.workflow_id.clone(),
                ctx.task_id.clone(),
                turn_id.map(|t| t.to_string()),
            ),
            None => (None, None, turn_id.map(|t| t.to_string())),
        };

        let interaction = UserInteraction {
            interaction_id: interaction_id.clone(),
            session_id: sid.to_string(),
            root_session_id,
            agent_id: _manifest.agent.id.clone(),
            turn_id: turn_id.unwrap_or("unknown").to_string(),
            kind,
            question: args.question,
            context: args.context,
            options,
            allow_freeform: args.allow_freeform,
            status: UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: now,
            answered_at: None,
            expires_at: None,
            workflow_id,
            task_id,
            checkpoint_turn_id,
        };

        if let Some(store) = gateway_store {
            store.create_user_interaction(&interaction)?;
            tracing::info!(
                target: "user_interaction",
                interaction_id = %interaction_id,
                session_id = %sid,
                "User interaction created; agent will suspend"
            );
            if let Some(ctx) = _run_context {
                if let Some(w) = &ctx.live_digest {
                    let opts_summary = if interaction.options.is_empty() {
                        None
                    } else {
                        Some(
                            interaction
                                .options
                                .iter()
                                .map(|o| format!("{}: {}", o.id, o.label))
                                .collect::<Vec<_>>()
                                .join("; "),
                        )
                    };
                    if let Ok(mut g) = w.lock() {
                        let _ = g.record_user_ask_pending(
                            &interaction.question,
                            opts_summary.as_deref(),
                        );
                    }
                }
            }
            if let Some(ctx) = _run_context {
                let _ = store.create_live_digest_event(
                    &crate::scheduler::gateway_store::LiveDigestEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        root_session_id: ctx.root_session_id.clone(),
                        source_session_id: ctx.session_id.clone(),
                        turn_id: turn_id.map(|s| s.to_string()),
                        source_agent_id: Some(_manifest.agent.id.clone()),
                        source_node_id: std::env::var("AUTONOETIC_NODE_ID")
                            .unwrap_or_else(|_| "gateway".to_string()),
                        event_type: "user.ask.pending".to_string(),
                        payload: Some(
                            serde_json::json!({
                                "interaction_id": interaction_id.clone(),
                                "question": crate::log_redaction::redact_text_for_logs(&interaction.question),
                                "options_count": interaction.options.len(),
                            })
                            .to_string(),
                        ),
                        created_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        } else {
            return Ok(serde_json::json!({
                "ok": false,
                "error_type": "resource",
                "message": "Gateway store not available; user.ask requires persistent store",
                "repair_hint": "Configure GatewayStore for this runtime before calling user.ask.",
                "error": "Gateway store not available; user.ask requires persistent store"
            })
            .to_string());
        }

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "interaction_required": true,
            "interaction_id": interaction_id,
            "status": "awaiting_user"
        }))
        .map_err(Into::into)
    }
}

pub struct UserInteractionStatusTool;

impl NativeTool for UserInteractionStatusTool {
    fn name(&self) -> &'static str {
        "user.interaction.status"
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
            Err(e) => serde_json::to_string(&serde_json::json!({
                "ok": false,
                "interaction_id": args.interaction_id,
                "error_type": "resource",
                "message": e.to_string(),
                "repair_hint": "Verify the interaction id and gateway store availability, then retry.",
                "error": e.to_string()
            }))
            .map_err(Into::into),
        }
    }
}
