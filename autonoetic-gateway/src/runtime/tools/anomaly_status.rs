//! `anomaly_status` — capability-free reporter-self read path for anomaly
//! flags (Ri-0.18 symmetry, #770 follow-up).
//!
//! [`AnomalyFlagTool`](crate::runtime::tools::anomaly_flag) lets a
//! zero-capability agent file a report, and the signed per-turn attestation
//! surfaces the reporter's still-pending flag summaries (#772 A.2). Without
//! a read path, a resumed model instance saw an unexplained id it could not
//! investigate and could not even re-read its own filing. This tool closes
//! the loop: any agent may read **its own** filings with zero capabilities.
//!
//! Two deliberate boundaries:
//!
//! - **Not a ledger oracle.** A flag filed by another agent is
//!   indistinguishable from an unknown id: both answer `not_found` with the
//!   `bad_reference` failure stamps. There is no cross-agent enumeration
//!   here — the ombudsman works the queue through its own office tooling,
//!   the operator through the JSON-RPC methods.
//! - **Adjudicator read-by-id.** A holder of
//!   [`Capability::AnomalyAdjudicate`] may read any flag by id: it is the
//!   designated decider (O-7) and otherwise adjudicates blind. Listing
//!   stays reporter-self; queue discovery for the office is a separate
//!   concern.

use std::path::Path;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::anomaly_flags::AnomalyFlag;

/// Upper bound on the list form — attestation-style bounding, keeping the
/// tool result inside the per-tool character budget.
const MAX_LISTED: usize = 32;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AnomalyStatusTool));
}

pub struct AnomalyStatusTool;

/// Every status a flag can be in: the filing default plus the decision
/// statuses. Pinned to the store's state machine by a unit test below.
const ALL_FLAG_STATUSES: &[&str] =
    &["pending", "under_review", "confirmed", "dismissed", "deferred"];

impl NativeTool for AnomalyStatusTool {
    fn name(&self) -> &'static str {
        "anomaly_status"
    }

    /// Capability-free by design — the mirror of `anomaly_flag` (Ri-0.18):
    /// the least-privileged witness that can file a report must be able to
    /// re-read its own report.
    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            // Keep this lean: the schema is paid on every turn of every
            // agent (capability-free tool), and the prompt-budget ratchet
            // (prompt_composition_budget) fails when prompt weight grows
            // invisibly.
            description: "Read your own anomaly flags (reports you filed with anomaly_flag). \
                No arguments lists your recent filings (id, status, severity, subject); pass \
                flag_id for the full record incl. your observation text and any decision. \
                Capability-free, like filing (Ri-0.18). Other agents' flags: not_found."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "flag_id": {
                        "type": "string",
                        "minLength": 1,
                        "description": "An 'aflag-…' id from your attestation line. Omit to list your filings."
                    },
                    "status": {
                        "type": "string",
                        "enum": ALL_FLAG_STATUSES,
                        "description": "Optional status filter for the list form."
                    }
                },
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
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            flag_id: Option<String>,
            #[serde(default)]
            status: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource(
                "GatewayStore not available — anomaly flags cannot be read",
                None::<String>,
            )
            .to_error_response());
        };

        if let Some(status) = args.status.as_deref() {
            if !ALL_FLAG_STATUSES.contains(&status) {
                return Ok(ToolError::validation(
                    format!("status must be one of: {}", ALL_FLAG_STATUSES.join(", ")),
                    None::<String>,
                )
                .to_error_response());
            }
        }

        let is_adjudicator = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::AnomalyAdjudicate { .. }));

        match args.flag_id.as_deref() {
            Some(flag_id) => {
                let flag_id = flag_id.trim();
                if flag_id.is_empty() {
                    return Ok(ToolError::validation(
                        "flag_id must not be empty",
                        Some("Pass the aflag-… id, or omit it to list your filings."),
                    )
                    .to_error_response());
                }
                let Some(flag) = store.get_anomaly_flag(flag_id)? else {
                    return Ok(self.flag_not_found(flag_id));
                };
                // Reporter-self read, or adjudicator read-by-id. Everyone
                // else gets the same not_found an unknown id gets — no
                // ledger-existence oracle.
                let viewer = if flag.reporter_agent_id == manifest.agent.id {
                    "reporter"
                } else if is_adjudicator {
                    "adjudicator"
                } else {
                    return Ok(self.flag_not_found(flag_id));
                };
                Ok(json!({
                    "ok": true,
                    "kind": "anomaly_flag",
                    "viewer": viewer,
                    "flag": flag_record(&flag),
                })
                .to_string())
            }
            None => {
                // Listing is reporter-self only — never widened by the
                // adjudicator capability (queue discovery is not a read of
                // one flag and stays out of band here).
                let flags =
                    store.list_anomaly_flags(args.status.as_deref(), Some(&manifest.agent.id), MAX_LISTED)?;
                let mut items: Vec<serde_json::Value> = flags
                    .iter()
                    .map(|f| flag_summary(f))
                    .collect();
                if items.len() == MAX_LISTED {
                    items.push(json!({
                        "note": format!("showing the {MAX_LISTED} most recent filings only")
                    }));
                }
                Ok(json!({
                    "ok": true,
                    "kind": "anomaly_flag_list",
                    "count": flags.len(),
                    "flags": items,
                })
                .to_string())
            }
        }
    }
}

impl AnomalyStatusTool {
    /// Not-found response — identical for unknown ids and other agents'
    /// flags, stamped `bad_reference` / non-retryable like resolve's
    /// deterministic lookup misses.
    fn flag_not_found(&self, flag_id: &str) -> String {
        let mut value = json!({
            "ok": false,
            "error_type": "resource",
            "error": "anomaly_flag_not_found",
            "message": format!("anomaly flag '{flag_id}' not found"),
            "repair_hint": "Check the id — it appears in your state attestation line while a \
                filing of yours is still pending. Call this tool with no arguments to list \
                your own filings.",
        });
        if let Some(object) = value.as_object_mut() {
            crate::runtime::failure_classification::WorkflowFailureMetadata::bad_reference()
                .apply_to_json_map(object);
        }
        value.to_string()
    }
}

/// Full record for a single read — includes the observation text the
/// reporter (or the adjudicator) is here for.
fn flag_record(flag: &AnomalyFlag) -> serde_json::Value {
    json!({
        "flag_id": flag.flag_id,
        "status": flag.status,
        "severity": flag.severity,
        "subject_ref": flag.subject_ref,
        "reporter_agent_id": flag.reporter_agent_id,
        "reporter_session_id": flag.reporter_session_id,
        "observation": flag.observation,
        "evidence": flag.evidence_json,
        "created_at": flag.created_at,
        "decision": flag.decision,
        "decision_reason": flag.decision_reason,
        "decided_by": flag.decided_by,
        "decided_at": flag.decided_at,
        "sla_breached_at": flag.sla_breached_at,
    })
}

/// Compact summary for the list form — no observation text, keeping the
/// response inside the tool-result budget.
fn flag_summary(flag: &AnomalyFlag) -> serde_json::Value {
    json!({
        "flag_id": flag.flag_id,
        "status": flag.status,
        "severity": flag.severity,
        "subject_ref": flag.subject_ref,
        "reporter_session_id": flag.reporter_session_id,
        "created_at": flag.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::anomaly_flags::FLAG_DECISION_STATUSES;

    /// The list-form status filter must never drift from the store's flag
    /// state machine (`pending` + the decision statuses). Order-insensitive:
    /// the schema enum is presentation, membership is the contract.
    #[test]
    fn all_flag_statuses_match_store_state_machine() {
        let mut actual: Vec<&str> = ALL_FLAG_STATUSES.to_vec();
        actual.sort_unstable();
        let mut expected: Vec<&str> = ["pending"]
            .iter()
            .copied()
            .chain(FLAG_DECISION_STATUSES.iter().copied())
            .collect();
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }
}
