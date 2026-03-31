use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::GatewayConfig;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SessionEscalateTool));
}

pub struct SessionEscalateTool;

impl NativeTool for SessionEscalateTool {
    fn name(&self) -> &'static str {
        "session.escalate"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Request help when stuck. Use this when you've tried reasonable approaches but cannot proceed correctly.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "Clear explanation of why you're stuck"
                    },
                    "context": {
                        "type": "string",
                        "description": "Relevant context: what you tried, what failed, error messages"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["reasoning_llm", "specialist", "human"],
                        "default": "reasoning_llm",
                        "description": "Who to ask for help"
                    },
                    "urgency": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "default": "medium"
                    },
                    "suggested_actions": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Possible next steps you're considering (helps target respond better)"
                    }
                },
                "required": ["reason", "context"],
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
            reason: String,
            context: String,
            #[serde(default = "default_target")]
            target: String,
            #[serde(default = "default_urgency")]
            urgency: String,
            #[serde(default)]
            suggested_actions: Option<Vec<String>>,
        }

        fn default_target() -> String {
            "reasoning_llm".to_string()
        }

        fn default_urgency() -> String {
            "medium".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let workflow_id = session_id
            .map(|sid| {
                let root = crate::runtime::content_store::root_session_id(sid);
                let agents_dir = agent_dir.parent().unwrap_or(agent_dir);
                let fallback_config = GatewayConfig {
                    agents_dir: agents_dir.to_path_buf(),
                    ..GatewayConfig::default()
                };
                let gw_config = config.unwrap_or(&fallback_config);
                crate::scheduler::resolve_workflow_id_for_root_session(gw_config, &root)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let suggested_actions = args.suggested_actions.clone().unwrap_or_default();

        let mut response = match args.target.as_str() {
            "reasoning_llm" => {
                serde_json::json!({
                    "escalation_type": "reasoning_llm",
                    "analysis": format!(
                        "Based on your situation:\n\nProblem: {}\n\nContext: {}\n\nSuggestions:\n1. Review your assumptions - check if you're working with correct data/parameters\n2. Break down the problem into smaller steps\n3. Consider alternative approaches you may have overlooked",
                        args.reason, args.context
                    ),
                    "confidence": "medium",
                    "next_steps": suggested_actions.clone()
                })
            }
            "specialist" => {
                serde_json::json!({
                    "escalation_type": "specialist",
                    "message": "To escalate to a specialist agent, use agent.spawn() with the appropriate specialist (e.g., 'researcher.default', 'architect.default', 'debugger.default')",
                    "suggested_specialists": [
                        "researcher.default - for information gathering and analysis",
                        "architect.default - for structural design and planning",
                        "debugger.default - for troubleshooting and root cause analysis",
                        "evaluator.default - for testing and validation",
                        "auditor.default - for security and compliance review"
                    ],
                    "original_reason": args.reason,
                    "original_context": args.context
                })
            }
            "human" => {
                serde_json::json!({
                    "escalation_type": "human",
                    "message": "This escalation has been logged. A human operator will review your request.",
                    "urgency": args.urgency,
                    "reason": args.reason,
                    "context": args.context,
                    "suggested_actions": suggested_actions.clone(),
                    "note": "You should EndTurn after escalating to human to allow them to review and respond."
                })
            }
            _ => {
                serde_json::json!({
                    "error": "Unknown escalation target",
                    "valid_targets": ["reasoning_llm", "specialist", "human"]
                })
            }
        };

        let event = autonoetic_types::workflow::WorkflowEventRecord {
            event_id: format!("esc-{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.clone(),
            task_id: None,
            event_type: "workflow.escalated".to_string(),
            agent_id: Some(manifest.agent.id.clone()),
            payload: serde_json::json!({
                "target": args.target,
                "urgency": args.urgency,
                "reason": args.reason,
                "context": args.context,
                "suggested_actions": suggested_actions,
            }),
            occurred_at: chrono::Utc::now().to_rfc3339(),
        };
        let _ = crate::scheduler::workflow_store::append_workflow_event(
            config.unwrap_or(&GatewayConfig::default()),
            gateway_store.as_deref(),
            &event,
        );

        response["escalation_id"] = serde_json::json!(event.event_id);
        response["workflow_id"] = serde_json::json!(workflow_id);

        serde_json::to_string(&response).map_err(Into::into)
    }
}
