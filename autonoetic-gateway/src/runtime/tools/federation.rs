use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::human_gate::{DecisionContext, GateKind, GateRequest, GateResult, GateService};
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{EscalationKind, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, RoleVerdictSummary};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(FederationEscalateTool));
}

#[derive(Debug, Deserialize)]
struct FederationEscalateArgs {
    escalation_id: Option<String>,
    #[serde(default)]
    artifact_id: String,
    #[serde(default)]
    artifact_ref: Option<String>,
    artifact_digest: Option<String>,
    agent_id: String,
    revision_id: String,
    role_verdicts: Vec<RoleVerdictSummary>,
    planner_synthesis: String,
    root_session_id: String,
}

pub struct FederationEscalateTool;

impl NativeTool for FederationEscalateTool {
    fn name(&self) -> &'static str {
        "federation_escalate"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::AgentSpawn { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Escalate federation jury verdicts to the operator for review. \
                 Call this after spawning all federation roles (static_evaluator, \
                 unit_test_runner, auditor) and collecting their verdicts via \
                 promotion_query. Construct an EscalationMessage with all role \
                 verdicts and your synthesis, then the operator will review and \
                 decide. Returns the escalation_id on success."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["agent_id", "revision_id", "role_verdicts", "planner_synthesis", "root_session_id"],
                "properties": {
                    "escalation_id": {
                        "type": "string",
                        "description": "Optional explicit ID (esc_xxxxxxxx). Auto-generated if omitted."
                    },
                    "artifact_ref": {
                        "type": "string",
                        "description": "Artifact ref (ar.*) from artifact_build or promotion_query response. Preferred over artifact_id."
                    },
                    "artifact_id": {
                        "type": "string",
                        "description": "Optional artifact identifier — the gateway resolves this from the revision record. Pass artifact_ref instead."
                    },
                    "artifact_digest": {
                        "type": "string",
                        "description": "Canonical digest of the artifact."
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "The agent being promoted."
                    },
                    "revision_id": {
                        "type": "string",
                        "description": "The revision being proposed for promotion."
                    },
                    "role_verdicts": {
                        "type": "array",
                        "description": "Verdicts from each federation role that evaluated.",
                        "items": {
                            "type": "object",
                            "required": ["role", "agent_id", "passed", "findings_summary", "recorded_at"],
                            "properties": {
                                "role": {
                                    "type": "string",
                                    "description": "Role name: static_evaluator, unit_test_runner, auditor, sealed_evaluator"
                                },
                                "agent_id": {"type": "string"},
                                "passed": {"type": "boolean"},
                                "findings_summary": {"type": "string"},
                                "evidence_ref": {"type": "string"},
                                "recorded_at": {"type": "string"}
                            }
                        }
                    },
                    "planner_synthesis": {
                        "type": "string",
                        "description": "Your summary and recommendation for the operator."
                    },
                    "root_session_id": {
                        "type": "string",
                        "description": "Root session this escalation belongs to."
                    }
                }
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: FederationEscalateArgs = serde_json::from_str(_arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(autonoetic_types::tool_error::ToolError::fatal(
                "Gateway store not available for federation.escalate",
                None::<String>,
            )
            .to_error_response());
        };

        // Resolve artifact_ref if provided, falling back to artifact_id if not.
        let caller_artifact_id = if let Some(ref ref_id) = args.artifact_ref {
            let sid = _session_id.unwrap_or("");
            store
                .resolve_artifact_ref_any_scope(ref_id, sid)?
                .map(|r| r.artifact_id)
                .unwrap_or_else(|| args.artifact_id.clone())
        } else {
            args.artifact_id.clone()
        };

        let (canonical_artifact_id, canonical_revision_id) = match store
            .get_agent_revision(&args.revision_id)?
        {
            Some(rev) => {
                let art = rev
                    .artifact_id
                    .as_deref()
                    .unwrap_or(&caller_artifact_id)
                    .to_string();
                if art != caller_artifact_id && !caller_artifact_id.is_empty() {
                    tracing::warn!(
                        target: "federation",
                        escalation_artifact_id = %caller_artifact_id,
                        canonical_artifact_id = %art,
                        "federation.escalate: correcting artifact id to canonical value from revision record"
                    );
                }
                (art, rev.revision_id.clone())
            }
            None => (caller_artifact_id.clone(), args.revision_id.clone()),
        };

        let escalation_id = args
            .escalation_id
            .unwrap_or_else(|| format!("esc_{:x}", uuid::Uuid::new_v4().as_u128()));

        let mut escalation = EscalationMessage::new(
            escalation_id.clone(),
            canonical_artifact_id.clone(),
            args.agent_id.clone(),
            canonical_revision_id.clone(),
            args.role_verdicts,
            args.planner_synthesis.clone(),
            args.root_session_id.clone(),
        );
        escalation.artifact_digest = args.artifact_digest;

        // Compute TTL from config.
        escalation.expires_at = _config.and_then(|c| {
            let ttl = c.escalation_timeout_secs;
            if ttl == 0 {
                None
            } else {
                Some((chrono::Utc::now() + chrono::Duration::seconds(ttl as i64)).to_rfc3339())
            }
        });

        if let (Some(gw_dir), artifact_id) = (gateway_dir, &canonical_artifact_id) {
            if !artifact_id.is_empty() {
                escalation.code_excerpts =
                    crate::runtime::code_excerpts::build_code_excerpts(artifact_id, gw_dir);
            }
        }

        // Build the promotion review as a single approval row, then project the
        // escalation row from it. This collapses the previous double-write and
        // gives #653 a typed discriminator (`EscalationKind::PromotionReview`)
        // instead of payload sniffing.
        let action = ScheduledAction::SessionEscalate {
            session_id: _session_id.unwrap_or("").to_string(),
            root_session_id: args.root_session_id.clone(),
            requested_by_agent_id: args.agent_id.clone(),
            reason: format!(
                "Promotion review for agent '{}' (escalation {})",
                args.agent_id, escalation_id
            ),
            context: args.planner_synthesis.clone(),
            urgency: "normal".to_string(),
            suggested_actions: vec!["approve".to_string(), "reject".to_string()],
            payload: Some(serde_json::json!({
                "escalation_id": escalation_id,
                "artifact_id": canonical_artifact_id,
                "revision_id": canonical_revision_id,
            })),
            kind: EscalationKind::PromotionReview,
        };

        let gate_service = GateService::new(store.clone());
        let gate_req = GateRequest {
            kind: GateKind::Approval {
                action: action.clone(),
                targets: Vec::new(),
                match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
            },
            manifest: _manifest,
            session_id: _session_id,
            run_context: _run_context,
            config: _config,
            context: DecisionContext::tier2(
                format!(
                    "Federation promotion review for agent {} revision {}",
                    args.agent_id, canonical_revision_id
                ),
                "Federation roles have recorded verdicts and the operator must approve promotion",
                format!(
                    "Artifact {} with {} role verdict(s). Promotion proceeds if approved; blocked if rejected.",
                    canonical_artifact_id,
                    escalation.role_verdicts.len()
                ),
                "Approve if all federation role verdicts support promotion; reject if any critical role failed or the synthesis is unconvincing",
            )
            .with_analysis(args.planner_synthesis.clone()),
            summary: format!(
                "Federation promotion review: agent '{}' artifact '{}' requires operator approval",
                args.agent_id, canonical_artifact_id
            ),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            request_id: None,
            turn_id: _turn_id,
        };

        match gate_service.check(gate_req)? {
            GateResult::AlreadyPending { gate_id, .. } | GateResult::Suspended { gate_id, .. } => {
                escalation.approval_request_id = Some(gate_id.clone());
                store.create_escalation(&mut escalation)?;
                Ok(serde_json::json!({
                    "ok": true,
                    "escalation_id": escalation_id,
                    "approval_request_id": gate_id,
                    "status": "pending",
                    "message": "Federation escalation created. The operator will review the verdicts via the approval system."
                })
                .to_string())
            }
            GateResult::Cleared { source, .. } => {
                // An identical promotion review was already approved (e.g. via
                // approval_ref or session grant). Reflect that in a resolved
                // escalation projection so admin.escalation_* stays consistent.
                escalation.status = EscalationStatus::Approved;
                escalation.approval_request_id = None;
                escalation.decided_by = Some("gate_service".to_string());
                escalation.resolved_at = Some(chrono::Utc::now().to_rfc3339());
                store.create_escalation(&mut escalation)?;
                Ok(serde_json::json!({
                    "ok": true,
                    "escalation_id": escalation_id,
                    "approval_request_id": serde_json::Value::Null,
                    "status": "approved",
                    "message": format!("An equivalent promotion review was already cleared ({:?}); escalation marked approved.", source)
                })
                .to_string())
            }
            GateResult::PolicyAllowed => {
                escalation.status = EscalationStatus::Approved;
                escalation.approval_request_id = None;
                escalation.decided_by = Some("policy".to_string());
                escalation.resolved_at = Some(chrono::Utc::now().to_rfc3339());
                store.create_escalation(&mut escalation)?;
                Ok(serde_json::json!({
                    "ok": true,
                    "escalation_id": escalation_id,
                    "approval_request_id": serde_json::Value::Null,
                    "status": "approved",
                    "message": "Policy allows this promotion review without operator approval; escalation marked approved."
                })
                .to_string())
            }
        }
    }
}
