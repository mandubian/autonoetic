//! `anomaly_adjudicate` — native ombudsman adjudication (citizenship RFC Part
//! F follow-up, #774).
//!
//! The ombudsman office holds the [`Capability::AnomalyAdjudicate`] right and
//! uses this tool to work the anomaly-flag queue directly: move a flag to
//! `under_review`, or record a terminal decision (`confirmed` / `dismissed` /
//! `deferred`). This removes the admin-proposal detour PR #829 introduced
//! ("file a recommendation as an admin proposal, operator enacts via
//! `anomaly.resolve`") while keeping the operator as the sovereignty
//! backstop:
//!
//! - The capability is granted explicitly to a named office, never via `*`.
//! - Terminal decisions require a non-empty `reason` (decider-obligation
//!   parity with the operator's `anomaly.resolve`).
//! - Terminal decisions additionally require an *exact* pattern grant (the
//!   status name), not `*` — they are authority-class operations a broad
//!   participation grant must not unlock.
//! - `anomaly_adjudication.require_terminal_cosign` defers terminal decisions
//!   back to the operator's `anomaly.resolve` when set.
//!
//! All transitions go through the existing [`GatewayStore::decide_anomaly_flag`]
//! path — no new SQL, no schema change. Every adjudication lands on the causal
//! chain like anyone else's action.

use std::path::Path;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::notification::{NotificationRecord, NotificationType};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::anomaly_flags::{
    FLAG_DECISION_STATUSES, FLAG_TERMINAL_DECISION_STATUSES,
};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AnomalyAdjudicateTool));
}

pub struct AnomalyAdjudicateTool;

impl NativeTool for AnomalyAdjudicateTool {
    fn name(&self) -> &'static str {
        "anomaly_adjudicate"
    }

    /// Office-scoped: only an agent holding the [`Capability::AnomalyAdjudicate`]
    /// right sees this tool. The capability is granted explicitly to a named
    /// office (e.g. `ombudsman.default`), never via `*` — the operator is the
    /// sovereignty backstop and can revoke the grant to fall back to manual
    /// adjudication via `anomaly.resolve`.
    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::AnomalyAdjudicate { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Apply an adjudication decision to an anomaly flag. Only an office \
                holding the AnomalyAdjudicate capability (typically the ombudsman) may call this. \
                Moves the flag to 'under_review' (non-terminal) or to a terminal decision \
                ('confirmed' | 'dismissed' | 'deferred'). Terminal decisions require a non-empty \
                reason and an exact capability grant for that status. The operator remains the \
                sovereignty backstop: revoke the capability or set \
                anomaly_adjudication.require_terminal_cosign to defer terminal decisions back to \
                anomaly.resolve."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "flag_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The anomaly flag id (e.g. 'aflag-…') to adjudicate."
                    },
                    "status": {
                        "type": "string",
                        "enum": FLAG_DECISION_STATUSES,
                        "description": "Decision to apply. 'under_review' is non-terminal; the \
                        others are terminal and stamp the decision fields."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Motivation for the decision. Required for terminal \
                        decisions (decider-obligation parity with anomaly.resolve); optional \
                        for 'under_review'."
                    },
                    "evidence_refs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional causal-event / trace / artifact refs that \
                        substantiate the decision. Recorded in the causal event payload."
                    }
                },
                "required": ["flag_id", "status"],
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
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            flag_id: String,
            status: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            evidence_refs: Vec<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        if args.flag_id.trim().is_empty() {
            return Ok(ToolError::validation(
                "flag_id must not be empty",
                Some("Provide the anomaly flag id, e.g. 'aflag-…'."),
            )
            .to_error_response());
        }
        if !FLAG_DECISION_STATUSES.contains(&args.status.as_str()) {
            return Ok(ToolError::validation(
                format!(
                    "status must be one of: {}",
                    FLAG_DECISION_STATUSES.join(", ")
                ),
                None::<String>,
            )
            .to_error_response());
        }

        let is_terminal = FLAG_TERMINAL_DECISION_STATUSES.contains(&args.status.as_str());

        // Authority check: terminal decisions require an *exact* pattern grant
        // for that status (confirmed/dismissed/deferred). A `*` participation
        // grant must NOT unlock terminal adjudication — mirrors the
        // `AuthorityOp` separation-of-powers discipline used elsewhere. Only
        // `under_review` is satisfied by a broad grant.
        let granted = manifest.capabilities.iter().any(|cap| {
            if let Capability::AnomalyAdjudicate { patterns } = cap {
                patterns.iter().any(|raw| {
                    let p = raw.trim();
                    if p.is_empty() {
                        return false;
                    }
                    if p == args.status {
                        return true;
                    }
                    // Broad grants cover only the non-terminal transition.
                    !is_terminal && (p == "*" || p == "under_review")
                })
            } else {
                false
            }
        });
        if !granted {
            return Ok(ToolError::permission(
                format!(
                    "AnomalyAdjudicate capability does not grant status '{}' for this office. \
                     Terminal decisions (confirmed/dismissed/deferred) require an exact pattern \
                     grant; 'under_review' is covered by a broad grant. Request the operator add \
                     the status to your AnomalyAdjudicate patterns, or use anomaly.resolve \
                     (operator JSON-RPC) for terminal decisions.",
                    args.status
                ),
            )
            .to_error_response());
        }

        // Operator co-sign: when enabled, terminal decisions are deferred back
        // to the operator's `anomaly.resolve`. The office still does the
        // analytical labor; it surfaces a recommendation, not a decision.
        let require_cosign = config
            .map(|c| c.anomaly_adjudication.require_terminal_cosign)
            .unwrap_or(false);
        if is_terminal && require_cosign {
            return Ok(ToolError::permission(
                format!(
                    "Terminal anomaly decision '{}' requires operator co-sign \
                     (anomaly_adjudication.require_terminal_cosign is enabled). The office \
                     recommendation must be enacted via anomaly.resolve. File the recommendation \
                     (e.g. via admin_proposal_create) and have the operator call \
                     anomaly.resolve, or disable require_terminal_cosign for this office.",
                    args.status
                ),
            )
            .to_error_response());
        }

        // Decider-obligation parity with anomaly.resolve (§O): a terminal
        // decision with no motivation is refused. The gateway checks presence
        // only, never quality (Lawful Executor).
        let has_reason = args
            .reason
            .as_deref()
            .map(|r| !r.trim().is_empty())
            .unwrap_or(false);
        if is_terminal && !has_reason {
            return Ok(ToolError::validation(
                format!(
                    "Terminal anomaly decision '{}' requires a non-empty reason \
                     (decider-obligation parity with anomaly.resolve / §O).",
                    args.status
                ),
                Some("Provide a motivation for the decision and retry."),
            )
            .to_error_response());
        }

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource(
                "GatewayStore not available — anomaly adjudication cannot be persisted",
                None::<String>,
            )
            .to_error_response());
        };

        // Reject unknown flags loudly — never a silent no-op. Fetch first so
        // the causal event can carry the reporter/subject for notification.
        let flag = match store.get_anomaly_flag(&args.flag_id)? {
            Some(f) => f,
            None => {
                return Ok(ToolError::not_found(
                    format!("Anomaly flag '{}' not found", args.flag_id),
                    Some("Check the flag id with anomaly.list_pending."),
                )
                .to_error_response());
            }
        };

        let decided = store.decide_anomaly_flag(
            &args.flag_id,
            &args.status,
            &manifest.agent.id,
            args.reason.as_deref(),
        )?;
        if !decided {
            return Ok(ToolError::not_found(
                format!("Anomaly flag '{}' not found", args.flag_id),
                None::<String>,
            )
            .to_error_response());
        }

        // Causal event + notification are best-effort visibility surfaces —
        // a failure there must not undo the durable decision above. Mirrors
        // the anomaly_flag.filed event shape: action = "decided" for terminal
        // decisions, "review_started" for the non-terminal `under_review`
        // transition. Both record under category "anomaly_flag".
        let action_label = if is_terminal { "decided" } else { "review_started" };
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("aflag-ev-{}", uuid::Uuid::new_v4()),
            agent_id: manifest.agent.id.clone(),
            session_id: session_id.unwrap_or_default().to_string(),
            turn_id: turn_id.map(str::to_string),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "anomaly_flag".to_string(),
            action: action_label.to_string(),
            status: args.status.clone(),
            enforced_rules: vec!["O-7".to_string()],
            target: Some(args.flag_id.clone()),
            payload: Some(
                json!({
                    "flag_id": &args.flag_id,
                    "decision": &args.status,
                    "decided_by": &manifest.agent.id,
                    "subject_ref": &flag.subject_ref,
                    "reporter_agent_id": &flag.reporter_agent_id,
                    "evidence_refs": &args.evidence_refs,
                    "terminal": is_terminal,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: args.reason.clone(),
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                "Failed to emit anomaly_flag.{action_label} causal event: {e}"
            );
        }

        // Notify the reporter that their flag reached a decision (Ri-0.5
        // spirit — voice that is used must not vanish silently).
        let notification = NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            NotificationType::AnomalyFlag,
            "system".to_string(),
            json!({
                "flag_id": &args.flag_id,
                "decision": &args.status,
                "decided_by": &manifest.agent.id,
                "subject_ref": &flag.subject_ref,
                "reporter_agent_id": &flag.reporter_agent_id,
                "terminal": is_terminal,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!("Failed to create anomaly adjudication notification: {e}");
        }

        Ok(json!({
            "ok": true,
            "flag_id": args.flag_id,
            "status": args.status,
            "decided_by": manifest.agent.id,
            "terminal": is_terminal,
            // The decided-upon content: the office's decision record must
            // show what was actually adjudicated (a blind decision is not
            // an adjudication, O-7).
            "subject_ref": flag.subject_ref,
            "reporter_agent_id": flag.reporter_agent_id,
            "reporter_session_id": flag.reporter_session_id,
            "severity": flag.severity,
            "observation": flag.observation,
            "evidence": flag.evidence_json,
            "filed_at": flag.created_at,
            "message": if is_terminal {
                format!("Anomaly flag '{}' recorded as '{}' (terminal). Decision is durable and non-repudiable.", args.flag_id, args.status)
            } else {
                format!("Anomaly flag '{}' moved to 'under_review' (non-terminal). A terminal decision is still owed (O-7).", args.flag_id)
            },
        })
        .to_string())
    }
}
