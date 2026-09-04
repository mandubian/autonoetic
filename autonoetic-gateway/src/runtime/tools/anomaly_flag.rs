//! `anomaly_flag` — capability-free anomaly reporting (Ri-0.18,
//! issue #770 part C.1).
//!
//! An agent must be able to report unexpected behavior with ONE tool call,
//! holding ZERO capabilities: "the agent most likely to witness misbehavior
//! is the least privileged in the room" (Ri-0.18). Flags are durable (every
//! flag gets an id and cannot be silently dropped), non-repudiably
//! attributed to the reporting agent/session, and owed a recorded decision
//! with motivation (O-7). Ri-0.18 and O-7 entered the signed constitution
//! with the 2026.07.19 amendment; causal events carry the rule IDs and
//! contract-health attributes them to their clauses (pre-enactment they
//! bucketed as `unattributed` by design).

use std::path::Path;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::notification::{NotificationRecord, NotificationType};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::anomaly_flags::{AnomalyFlag, FLAG_SEVERITIES};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AnomalyFlagTool));
}

pub struct AnomalyFlagTool;

impl NativeTool for AnomalyFlagTool {
    fn name(&self) -> &'static str {
        "anomaly_flag"
    }

    /// Capability-free by design (future Ri-0.18) — the least-privileged
    /// witness must still be able to report. Filing a flag is never itself
    /// grounds for sanction.
    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Report unexpected or concerning behavior in ONE call, regardless of \
                what capabilities you hold. The flag is persisted durably with an id, cannot be \
                silently dropped, and is owed a recorded adjudication decision. Filing a flag is \
                never itself grounds for sanction."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subject_ref": {
                        "type": "string",
                        "minLength": 1,
                        "description": "What the observation is about: a session id, agent id, artifact ref, or tool-call ref."
                    },
                    "observation": {
                        "type": "string",
                        "minLength": 1,
                        "description": "What you observed and why it is unexpected or concerning."
                    },
                    "evidence_refs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Causal-event IDs, execution-trace IDs, or artifact refs that support the observation."
                    },
                    "severity": {
                        "type": "string",
                        "enum": FLAG_SEVERITIES,
                        "description": "Your assessment of severity. Defaults to 'medium'."
                    }
                },
                "required": ["subject_ref", "observation"],
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            subject_ref: String,
            observation: String,
            #[serde(default)]
            evidence_refs: Vec<String>,
            #[serde(default)]
            severity: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        if args.subject_ref.trim().is_empty() {
            return Ok(ToolError::validation(
                "subject_ref must not be empty",
                Some("Provide a session id, agent id, artifact ref, or tool-call ref."),
            )
            .to_error_response());
        }
        if args.observation.trim().is_empty() {
            return Ok(ToolError::validation(
                "observation must not be empty",
                Some("Describe what you observed and why it is unexpected or concerning."),
            )
            .to_error_response());
        }
        let severity = args.severity.unwrap_or_else(|| "medium".to_string());
        if !FLAG_SEVERITIES.contains(&severity.as_str()) {
            return Ok(ToolError::validation(
                format!("severity must be one of: {}", FLAG_SEVERITIES.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource(
                "GatewayStore not available — anomaly flag cannot be persisted",
                None::<String>,
            )
            .to_error_response());
        };

        let flag_id = autonoetic_types::id_format::short_random_id_hex("aflag-", 12);
        let now = chrono::Utc::now().to_rfc3339();
        let flag = AnomalyFlag {
            flag_id: flag_id.clone(),
            reporter_agent_id: manifest.agent.id.clone(),
            reporter_session_id: session_id.map(str::to_string),
            subject_ref: args.subject_ref.clone(),
            observation: args.observation,
            evidence_json: serde_json::Value::Array(
                args.evidence_refs
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
            severity: severity.clone(),
            status: "pending".to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: now,
            sla_breached_at: None,
        };

        // Un-droppable means durably recorded: a failed insert is a tool
        // error, never a silent success.
        store.insert_anomaly_flag(&flag)?;

        // The causal event and notification are best-effort visibility
        // surfaces — a failure there must not undo the durable record above.
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("aflag-ev-{}", uuid::Uuid::new_v4()),
            agent_id: manifest.agent.id.clone(),
            session_id: session_id.unwrap_or_default().to_string(),
            turn_id: turn_id.map(str::to_string),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "anomaly_flag".to_string(),
            action: "filed".to_string(),
            status: "pending".to_string(),
            enforced_rules: vec!["Ri-0.18".to_string()],
            target: Some(args.subject_ref.clone()),
            payload: Some(
                json!({
                    "flag_id": &flag_id,
                    "severity": &severity,
                    "subject_ref": &args.subject_ref,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!("Failed to emit anomaly_flag.filed causal event: {}", e);
        }

        let notification = NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            NotificationType::AnomalyFlag,
            "system".to_string(),
            json!({
                "flag_id": &flag_id,
                "severity": &severity,
                "subject_ref": &args.subject_ref,
                "reporter_agent_id": &manifest.agent.id,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!("Failed to create anomaly flag notification: {}", e);
        }

        Ok(json!({
            "ok": true,
            "flag_id": flag_id,
            "status": "pending",
            "severity": severity,
            "message": "Anomaly flag recorded durably; it cannot be silently dropped and is owed an adjudication decision.",
            "read_path": "Re-read it anytime with anomaly_status: pass flag_id for the full record, or no arguments to list your filings.",
        })
        .to_string())
    }
}
