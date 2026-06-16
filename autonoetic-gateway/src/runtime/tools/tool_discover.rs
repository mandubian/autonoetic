use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(ToolDiscoverTool));
}

pub struct ToolDiscoverTool;

#[derive(Debug, Deserialize)]
struct ToolDiscoverArgs {
    tools: Vec<String>,
}

impl NativeTool for ToolDiscoverTool {
    fn name(&self) -> &'static str {
        "tool_discover"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Request additional tools by name or prefix pattern (e.g. 'scheduler_cron_*', \
                          'credential_setup'). Discovered tools become available on subsequent turns. \
                          Returns the list of patterns that were accepted."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tool names or prefix patterns (ending with *) to discover."
                    }
                },
                "required": ["tools"],
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
        _gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ToolDiscoverArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("invalid arguments for tool_discover: {e}"))?;

        if args.tools.is_empty() {
            return Ok(serde_json::json!({
                "ok": true,
                "discovered": [],
                "message": "No tools requested."
            }).to_string());
        }

        let Some(ctx) = run_context else {
            return Ok(ToolError::execution("No run context available.", Some("Ensure the tool is invoked within an active session context.")).with_code("no_run_context").to_error_response());
        };

        let Some(writer) = &ctx.discovered_tools else {
            return Ok(ToolError::execution("Discovered-tools writer not available.", Some("Ensure the discovery subsystem is initialized.")).with_code("discovery_writer_unavailable").to_error_response());
        };

        let mut accepted: Vec<String> = Vec::new();
        {
            let mut set = writer.lock().unwrap_or_else(|e| e.into_inner());
            for pattern in &args.tools {
                let p = pattern.trim();
                if !p.is_empty() {
                    set.insert(p.to_string());
                    accepted.push(p.to_string());
                }
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "discovered": accepted,
            "message": format!("{} pattern(s) accepted — matching tools will appear on the next turn.", accepted.len())
        }).to_string())
    }
}
