//! Native tool: aggregate recent quality_signal Tier-2 memories for evolution workflows.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::quality_signal::build_quality_trend_report;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(QualityTrendReportTool));
}

pub struct QualityTrendReportTool;

impl NativeTool for QualityTrendReportTool {
    fn name(&self) -> &'static str {
        "quality_trend_report"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        use autonoetic_types::capability::Capability;
        manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                Capability::ReadAccess { .. }
                    | Capability::Evaluation { .. }
                    | Capability::AgentSpawn { .. }
                    | Capability::SchedulerAccess { .. }
            )
        })
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Aggregate recent per-session quality_signal Tier-2 memories into per-agent trend metrics (average score, errors, approvals, completion rate). Used by evolution orchestrators to prioritize agents for improvement. Fails explicitly when no quality_signal data exists.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_limit": {
                        "type": "integer",
                        "description": "Max quality_signal memories to scan (default 200, max 500)."
                    },
                    "agent_id_prefix": {
                        "type": "string",
                        "description": "Optional filter: include sessions whose agent_id equals this string or contains it as substring."
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
            memory_limit: Option<i64>,
            #[serde(default)]
            agent_id_prefix: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            anyhow::bail!("quality_trend_report requires GatewayStore");
        };

        let limit = args.memory_limit.unwrap_or(200).clamp(1, 500) as usize;
        let filter = args
            .agent_id_prefix
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let report = build_quality_trend_report(store.as_ref(), limit, filter)?;

        Ok(report.to_string())
    }
}
