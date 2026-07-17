use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::prompt_budget::tool_matches_discovered_pattern;
use crate::runtime::tool_call_processor::canonical_tool_name;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(ToolDiscoverTool));
}

pub struct ToolDiscoverTool;

#[derive(Debug, PartialEq, Eq)]
enum DiscoverStatus {
    Available,
    UnavailableDueToCapability,
    Unmatched,
}

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
            description: "Request additional native tools by name or prefix pattern (e.g. 'scheduler_cron_*', \
                          'credential_setup'). Returns separate available, unavailable_due_to_capability, and \
                          unmatched lists. Only available patterns are added on subsequent turns."
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
                "available": [],
                "unavailable_due_to_capability": [],
                "unmatched": [],
                "message": "No tools requested."
            }).to_string());
        }

        let Some(ctx) = run_context else {
            return Ok(ToolError::execution("No run context available.", Some("Ensure the tool is invoked within an active session context.")).with_code("no_run_context").to_error_response());
        };

        let Some(writer) = &ctx.discovered_tools else {
            return Ok(ToolError::execution("Discovered-tools writer not available.", Some("Ensure the discovery subsystem is initialized.")).with_code("discovery_writer_unavailable").to_error_response());
        };
        let Some(catalog) = &ctx.tool_discovery_catalog else {
            return Ok(ToolError::execution("Tool discovery catalog not available.", Some("Ensure the discovery subsystem is initialized.")).with_code("discovery_catalog_unavailable").to_error_response());
        };

        let mut available: Vec<String> = Vec::new();
        let mut unavailable_due_to_capability: Vec<String> = Vec::new();
        let mut unmatched: Vec<String> = Vec::new();
        {
            let mut set = writer.lock().unwrap_or_else(|e| e.into_inner());
            for pattern in &args.tools {
                let normalized = normalize_tool_pattern(pattern);
                let p = normalized.trim();
                if !p.is_empty() {
                    match classify_pattern(catalog, p) {
                        DiscoverStatus::Available => {
                            set.insert(p.to_string());
                            available.push(p.to_string());
                        }
                        DiscoverStatus::UnavailableDueToCapability => {
                            unavailable_due_to_capability.push(p.to_string());
                        }
                        DiscoverStatus::Unmatched => unmatched.push(p.to_string()),
                    }
                }
            }
        }

        let message = format!(
            "{} available, {} unavailable due to capability, {} unmatched.",
            available.len(),
            unavailable_due_to_capability.len(),
            unmatched.len()
        );
        Ok(serde_json::json!({
            "ok": true,
            "discovered": available.clone(),
            "available": available,
            "unavailable_due_to_capability": unavailable_due_to_capability,
            "unmatched": unmatched,
            "message": message
        }).to_string())
    }
}

fn normalize_tool_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();
    if let Some(prefix) = trimmed.strip_suffix('*') {
        format!("{}*", canonical_tool_name(prefix))
    } else {
        canonical_tool_name(trimmed).to_string()
    }
}

fn classify_pattern(
    catalog: &crate::runtime::active_execution_registry::NativeToolDiscoveryCatalog,
    pattern: &str,
) -> DiscoverStatus {
    if catalog
        .available
        .iter()
        .any(|name| tool_matches_discovered_pattern(name, pattern))
    {
        DiscoverStatus::Available
    } else if catalog
        .registered
        .iter()
        .any(|name| tool_matches_discovered_pattern(name, pattern))
    {
        DiscoverStatus::UnavailableDueToCapability
    } else {
        DiscoverStatus::Unmatched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::active_execution_registry::NativeToolDiscoveryCatalog;
    use std::collections::HashSet;

    #[test]
    fn normalizes_dotted_execution_tool_aliases() {
        assert_eq!(normalize_tool_pattern("sandbox.exec"), "sandbox_exec");
        assert_eq!(normalize_tool_pattern("artifact.exec"), "artifact_exec");
        assert_eq!(normalize_tool_pattern("artifact.prepare"), "artifact_prepare");
    }

    #[test]
    fn classifies_available_forbidden_and_unmatched_patterns() {
        let catalog = NativeToolDiscoveryCatalog {
            registered: ["sandbox_exec", "artifact_exec"]
                .into_iter()
                .map(str::to_string)
                .collect::<HashSet<_>>(),
            available: ["artifact_exec"]
                .into_iter()
                .map(str::to_string)
                .collect::<HashSet<_>>(),
        };
        assert_eq!(
            classify_pattern(&catalog, "artifact_exec"),
            DiscoverStatus::Available
        );
        assert_eq!(
            classify_pattern(&catalog, "sandbox_exec"),
            DiscoverStatus::UnavailableDueToCapability
        );
        assert_eq!(
            classify_pattern(&catalog, "missing_tool"),
            DiscoverStatus::Unmatched
        );
    }
}
