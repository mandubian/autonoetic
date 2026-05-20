use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::TrajectoryConfig;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;
use std::sync::atomic::Ordering;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SentinelSuppressTool));
}

pub struct SentinelSuppressTool;

impl NativeTool for SentinelSuppressTool {
    fn name(&self) -> &'static str {
        "sentinel_suppress"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Suppress divergence monitoring messages for the specified number of turns. Use this when you are already aware of a divergence pattern and do not need repeated planner notifications. The suppression is bounded by the gateway configuration (`trajectory.suppress_max_turns`).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "turns": {
                        "type": "integer",
                        "description": "Number of turns to suppress divergence messages for (clamped to suppress_max_turns)",
                        "minimum": 1
                    }
                },
                "required": ["turns"],
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
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            turns: u32,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        if args.turns == 0 {
            return Ok(ToolError::validation(
                "turns must be >= 1".to_string(),
                None::<String>,
            )
            .to_error_response());
        }

        let max_turns = config
            .map(|c| c.trajectory.suppress_max_turns)
            .unwrap_or(TrajectoryConfig::default().suppress_max_turns);
        let clamped = args.turns.min(max_turns);

        let current_turn: u64 = match turn_id.and_then(|id| id.strip_prefix("turn-")).and_then(|s| s.parse().ok()) {
            Some(t) => t,
            None => {
                return Ok(serde_json::json!({
                    "ok": false,
                    "error_type": "internal",
                    "error": "invalid_turn_context",
                    "message": "sentinel.suppress requires a valid turn_id (turn-<N>)".to_string(),
                }).to_string());
            }
        };

        let suppress_until = current_turn + clamped as u64;

        let target = run_context
            .and_then(|ctx| ctx.sentinel_suppress_target.as_ref())
            .ok_or_else(|| anyhow::anyhow!("sentinel.suppress: suppression target unavailable (no active session context)"))?;

        target.store(suppress_until, Ordering::Release);
        tracing::debug!(
            target: "autonoetic::trajectory",
            current_turn,
            clamped,
            suppress_until,
            "sentinel.suppress activated"
        );

        Ok(serde_json::json!({
            "ok": true,
            "suppressed_for_turns": clamped,
            "suppress_until_turn": suppress_until,
            "current_turn": current_turn,
            "max_allowed": max_turns,
        })
        .to_string())
    }
}
