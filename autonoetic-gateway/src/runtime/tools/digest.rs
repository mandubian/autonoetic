use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(DigestAnnotateTool));
}

pub struct DigestAnnotateTool;

impl NativeTool for DigestAnnotateTool {
    fn name(&self) -> &'static str {
        "digest_annotate"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Add a reasoning, decision, observation, or lesson line to the live session digest (markdown file). Use for audit trail and handoff context without bloating the model transcript.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["reasoning", "decision", "observation", "lesson"],
                        "description": "Category of annotation"
                    },
                    "content": {
                        "type": "string",
                        "description": "Text to record in the digest"
                    }
                },
                "required": ["type", "content"],
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
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(rename = "type")]
            annotation_type: String,
            content: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        let allowed = ["reasoning", "decision", "observation", "lesson"];
        if !allowed.contains(&args.annotation_type.as_str()) {
            return Ok(ToolError::validation(
                format!("type must be one of: {}", allowed.join(", ")),
                None::<String>,
            ).to_error_response());
        }
        if let Some(ctx) = run_context {
            if let Some(w) = &ctx.live_digest {
                if let Ok(mut g) = w.lock() {
                    g.record_annotation(&args.annotation_type, &args.content)?;
                }
            }
            if let Some(w) = &ctx.live_report {
                if let Ok(mut g) = w.lock() {
                    let _ =
                        g.record_annotation(&args.annotation_type, &args.content, _turn_id);
                }
            }
            if let Some(store) = _gateway_store.as_ref() {
                let _ = store.create_live_digest_event(
                    &crate::scheduler::gateway_store::LiveDigestEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        root_session_id: ctx.root_session_id.clone(),
                        source_session_id: ctx.session_id.clone(),
                        turn_id: _turn_id.map(|s| s.to_string()),
                        source_agent_id: Some(ctx.agent_id.clone()),
                        source_node_id: std::env::var("AUTONOETIC_NODE_ID")
                            .unwrap_or_else(|_| "gateway".to_string()),
                        event_type: "digest_annotate".to_string(),
                        payload: Some(
                            serde_json::json!({
                                "type": args.annotation_type,
                                "content": crate::log_redaction::redact_text_for_logs(&args.content),
                            })
                            .to_string(),
                        ),
                        created_at: chrono::Utc::now().to_rfc3339(),
                    },
                );
            }
        }
        Ok(serde_json::to_string(&serde_json::json!({ "ok": true }))?)
    }
}
