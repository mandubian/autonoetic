use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::causal_chain::{default_enforced_rules, CausalEventRecord};
use autonoetic_types::config::TrajectoryConfig;
use autonoetic_types::tool_error::ToolError;
use chrono::Utc;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
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
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional explanation for why suppression is being requested"
                    }
                },
                "required": ["turns"],
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
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            turns: u32,
            #[serde(default)]
            reason: Option<String>,
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
                return Ok(ToolError::resource(
                    "sentinel.suppress requires a valid turn_id (turn-<N>) — no active turn context",
                    Some("invoke this tool only from within an agent turn"),
                )
                .to_error_response());
            }
        };

        let suppress_until = current_turn + clamped as u64;
        let reason = args.reason.clone();

        let target = match run_context.and_then(|ctx| ctx.sentinel_suppress_target.as_ref()) {
            Some(t) => t,
            None => {
                return Ok(ToolError::resource(
                    "sentinel.suppress: suppression target unavailable (no active session context)",
                    Some("ensure the tool is invoked from a live agent session"),
                )
                .to_error_response());
            }
        };

        target.store(suppress_until, Ordering::Release);
        tracing::debug!(
            target: "autonoetic::trajectory",
            current_turn,
            clamped,
            suppress_until,
            reason = reason.as_deref().unwrap_or(""),
            "sentinel.suppress activated"
        );

        // Emit causal event for suppression activation
        if let Some(ref store) = gateway_store {
            let now = Utc::now();
            let event = CausalEventRecord {
                event_id: uuid::Uuid::new_v4().to_string(),
                agent_id: manifest.agent.id.clone(),
                session_id: session_id.unwrap_or_default().to_string(),
                turn_id: turn_id.map(|s| s.to_string()),
                event_seq: now.timestamp_millis().max(0) as u64,
                timestamp: now.to_rfc3339(),
                category: "sentinel".to_string(),
                action: "suppress_activated".to_string(),
                status: "SUCCESS".to_string(),
                enforced_rules: default_enforced_rules(),
                target: None,
                payload: Some(
                    serde_json::json!({
                        "suppressed_for_turns": clamped,
                        "suppress_until_turn": suppress_until,
                        "current_turn": current_turn,
                        "max_allowed": max_turns,
                        "reason": reason.clone(),
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: reason.clone(),
            };
            if let Err(e) = store.create_causal_event(&event) {
                tracing::warn!(target: "autonoetic::trajectory", error = %e, "Failed to log sentinel.suppress_activated causal event");
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "suppressed_for_turns": clamped,
            "suppress_until_turn": suppress_until,
            "current_turn": current_turn,
            "max_allowed": max_turns,
            "reason": reason,
        })
        .to_string())
    }
}
