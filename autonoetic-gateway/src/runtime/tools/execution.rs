use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::disclosure::ViewerClass;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ExecutionSearchTool));
}

pub struct ExecutionSearchTool;

impl NativeTool for ExecutionSearchTool {
    fn name(&self) -> &'static str {
        "execution_search"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search raw execution traces for tool-level debugging within sessions. Query by tool name, success status, error type, command pattern, or agent ID. Returns execution metadata including exit codes, duration, and error info. For cross-session discovery of high-level session summaries, use observability.search instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Filter by tool name (e.g., 'sandbox.exec'). Optional."
                    },
                    "success": {
                        "type": "boolean",
                        "description": "Filter by success (true), failure (false), or both (null). Optional."
                    },
                    "error_type": {
                        "type": "string",
                        "enum": ["compilation", "runtime", "permission", "timeout", "validation", "resource", "conflict", "quota_exceeded", "not_found"],
                        "description": "Filter by error type. Optional."
                    },
                    "command_pattern": {
                        "type": "string",
                        "description": "Filter by command pattern (SQL LIKE). Optional."
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Filter by agent ID. Optional."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Restrict to this session id and nested sessions (exact match or id/<suffix>). Optional."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 10)."
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
            #[serde(default)]
            tool_name: Option<String>,
            #[serde(default)]
            success: Option<bool>,
            #[serde(default)]
            error_type: Option<String>,
            #[serde(default)]
            command_pattern: Option<String>,
            #[serde(default)]
            agent_id: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            limit: Option<i64>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource("execution.search requires GatewayStore to be configured", None::<String>).to_error_response());
        };

        let limit = args.limit.unwrap_or(10).min(100) as i64;

        let viewer = ViewerClass::Agent;

        let traces = store.search_execution_traces(
            args.tool_name.as_deref(),
            args.success,
            args.error_type.as_deref(),
            args.command_pattern.as_deref(),
            args.agent_id.as_deref(),
            args.session_id.as_deref(),
            limit,
        )?;

        let items: Vec<serde_json::Value> = traces
            .into_iter()
            .map(|t| t.to_json_for_viewer(viewer))
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}
