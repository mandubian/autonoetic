use crate::causal_chain::CausalLogger;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::promotion_store::PromotionStore;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::promotion::{
    PromotionQueryArgs, PromotionQueryResponse, PromotionRecordArgs, PromotionRecordResponse,
};
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(PromotionRecordTool));
    registry.register(Box::new(PromotionQueryTool));
}

/// Agents that may invoke [`PromotionRecordTool`] (when the tool surface includes it).
///
/// Used by [`crate::runtime::tool_dispatch::child_tool_tier_filter_for_manifest`] to set
/// [`crate::runtime::tools::ToolTierFilter::allow_promotion_record_without_specialized_tier`]
/// for delegated sessions: `promotion_record` is a Specialized-tier tool, and child
/// sessions would otherwise omit the entire Specialized tier.
pub fn manifest_may_record_promotion_verdicts(manifest: &AgentManifest) -> bool {
    matches!(
        manifest.agent.id.as_str(),
        "sealed_evaluator.default"
            | "auditor.default"
            | "static_evaluator.default"
            | "unit_test_runner.default"
    )
}

fn is_promotion_agent(manifest: &AgentManifest) -> bool {
    manifest_may_record_promotion_verdicts(manifest)
}

pub struct PromotionRecordTool;

impl NativeTool for PromotionRecordTool {
    fn name(&self) -> &'static str {
        "promotion_record"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Records promotion status (auditor/static_evaluator/unit_test_runner/sealed_evaluator validation result) for an artifact. Only authorized promotion agents can call this tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Canonical artifact ID (e.g., 'art_a1b2c3d4'). Alternative: use artifact_ref."
                    },
                    "artifact_ref": {
                        "type": "string",
                        "description": "Short artifact ref (e.g., 'ar.386f5b222421'). Resolved server-side to the canonical artifact_id. Alternative to artifact_id."
                    },
                    "artifact_digest": {
                        "type": "string",
                        "description": "Optional SHA-256 digest of the artifact for integrity verification"
                    },
                    "role": {
                        "type": "string",
                        "description": "Role recording this promotion",
                        "enum": ["evaluator", "auditor", "static_evaluator", "unit_test_runner", "sealed_evaluator"]
                    },
                    "pass": {
                        "type": "boolean",
                        "description": "Whether this validation passed (true) or failed (false)"
                    },
                    "findings": {
                        "type": "array",
                        "description": "Findings from this validation",
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": {
                                    "type": "string",
                                    "enum": ["info", "warning", "error", "critical"]
                                },
                                "description": { "type": "string" },
                                "evidence": { "type": "string" }
                            },
                            "required": ["severity", "description"]
                        }
                    },
                    "summary": {
                        "type": "string",
                        "description": "Human-readable summary of the validation result"
                    }
                },
                "required": ["role", "pass"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        is_promotion_agent(manifest)
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: PromotionRecordArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        // Resolve artifact_id from artifact_ref if needed.
        let artifact_id = match (&args.artifact_id, &args.artifact_ref) {
            (Some(id), _) if id.starts_with("art_") => id.clone(),
            (_, Some(ref_id)) => {
                let Some(gs) = gateway_store.as_ref() else {
                    anyhow::bail!("promotion_record: artifact_ref requires GatewayStore");
                };
                let Some(sid) = session_id else {
                    anyhow::bail!("promotion_record: artifact_ref requires a session_id");
                };
                let record = gs
                    .resolve_artifact_ref_any_scope(ref_id, sid)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "promotion_record: artifact_ref '{}' not found, expired, or revoked",
                            ref_id
                        )
                    })?;
                record.artifact_id
            }
            _ => anyhow::bail!(
                "promotion_record: either artifact_id (starting with 'art_') or artifact_ref is required"
            ),
        };

        anyhow::ensure!(
            args.content_digest.is_none(),
            "content_digest is gateway-owned and must not be provided to promotion.record"
        );

        let findings_with_errors: Vec<String> = args
            .findings
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let mut errs = Vec::new();
                if f.description.trim().is_empty() {
                    errs.push("description is empty");
                }
                if !errs.is_empty() {
                    Some(format!("findings[{}]: {}", i, errs.join(", ")))
                } else {
                    None
                }
            })
            .collect();
        if !findings_with_errors.is_empty() {
            return Ok(ToolError::validation(
                format!(
                    "Findings schema validation failed:\n  - {}",
                    findings_with_errors.join("\n  - ")
                ),
                None::<String>,
            ).to_error_response());
        }

        let has_error_or_critical = args.findings.iter().any(|f| {
            matches!(
                f.severity,
                autonoetic_types::promotion::FindingSeverity::Error
                    | autonoetic_types::promotion::FindingSeverity::Critical
            )
        });

        if args.pass && has_error_or_critical {
            return Ok(ToolError::validation(
                "Cannot set pass=true when findings contain 'error' or 'critical' severity. Fix the issues and re-evaluate, or set pass=false.",
                None::<String>,
            ).to_error_response());
        }

        if args.pass {
            let warnings_without_evidence: Vec<String> = args
                .findings
                .iter()
                .filter(|f| {
                    matches!(
                        f.severity,
                        autonoetic_types::promotion::FindingSeverity::Warning
                    ) && f.evidence.as_ref().map_or(true, |e| e.trim().is_empty())
                })
                .map(|f| {
                    let desc = &f.description;
                    if desc.len() > 80 {
                        let end = desc.floor_char_boundary(80);
                        format!("{}...", &desc[..end])
                    } else {
                        desc.clone()
                    }
                })
                .collect();
            if !warnings_without_evidence.is_empty() {
                return Ok(ToolError::validation(
                    format!(
                        "Cannot set pass=true when warning findings lack evidence. \
                         The following warnings need concrete evidence (e.g., sandbox output, test results) \
                         to prove the issue was investigated:\n  - {}",
                        warnings_without_evidence.join("\n  - ")
                    ),
                    None::<String>,
                ).to_error_response());
            }
        }

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Promotion store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let causal_log_path = gw_dir.join("history").join("causal_chain.jsonl");
        if let Some(parent) = causal_log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Enforce audit-first ordering: no promotion DB mutation without a durable causal append.
        let logger = CausalLogger::new(&causal_log_path)?;
        logger.log_durable(
            &manifest.agent.id,
            session_id.unwrap_or("unknown"),
            turn_id,
            0,
            "tool",
            "promotion_record",
            EntryStatus::Success,
            Some(crate::log_redaction::RedactedPayload::from_raw(
                serde_json::json!({
                    "arguments": {
                        "artifact_id": artifact_id,
                        "role": args.role.as_str(),
                        "pass": args.pass,
                    }
                }),
            )),
        )?;

        let store = PromotionStore::new(gw_dir)?;

        let record = store.record_promotion(
            artifact_id.clone(),
            args.artifact_digest.clone(),
            None,
            args.role.clone(),
            &manifest.agent.id,
            args.pass,
            args.findings.clone(),
            args.summary.clone(),
        )?;

        let response = PromotionRecordResponse {
            ok: true,
            promotion_record: record,
        };

        serde_json::to_string(&response).map_err(Into::into)
    }
}

pub struct PromotionQueryTool;

impl NativeTool for PromotionQueryTool {
    fn name(&self) -> &'static str {
        "promotion_query"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Queries the promotion status of an artifact. Returns all role validation results (evaluator, auditor, static_evaluator, unit_test_runner, sealed_evaluator), or null if no promotion record exists.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Canonical artifact ID (e.g., 'art_a1b2c3d4'). Alternative: use artifact_ref."
                    },
                    "artifact_ref": {
                        "type": "string",
                        "description": "Short artifact ref (e.g., 'ar.386f5b222421'). Resolved server-side to the canonical artifact_id. Alternative to artifact_id."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|cap| {
            matches!(
                cap,
                autonoetic_types::capability::Capability::ReadAccess { .. }
            )
        })
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: PromotionQueryArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let artifact_id = match (&args.artifact_id, &args.artifact_ref) {
            (id, _) if id.starts_with("art_") => id.clone(),
            (_, Some(ref_id)) => {
                let Some(gs) = _gateway_store.as_ref() else {
                    anyhow::bail!("promotion_query: artifact_ref requires GatewayStore");
                };
                let Some(sid) = _session_id else {
                    anyhow::bail!("promotion_query: artifact_ref requires a session_id");
                };
                let record = gs
                    .resolve_artifact_ref_any_scope(ref_id, sid)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "promotion_query: artifact_ref '{}' not found, expired, or revoked",
                            ref_id
                        )
                    })?;
                record.artifact_id
            }
            _ => anyhow::bail!(
                "promotion_query: either artifact_id (starting with 'art_') or artifact_ref is required"
            ),
        };

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Promotion store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let store = PromotionStore::new(gw_dir)?;

        let response = match store.get_promotion(&artifact_id) {
            Some(record) => PromotionQueryResponse {
                artifact_id: record.artifact_id,
                content_digest: record.content_digest,
                evaluator_pass: Some(record.evaluator_pass),
                auditor_pass: Some(record.auditor_pass),
                evaluator_id: record.evaluator_id,
                auditor_id: record.auditor_id,
                evaluator_findings: record.evaluator_findings,
                auditor_findings: record.auditor_findings,
                evaluator_timestamp: record.evaluator_timestamp,
                auditor_timestamp: record.auditor_timestamp,
                static_evaluator_pass: Some(record.static_evaluator_pass),
                static_evaluator_id: record.static_evaluator_id,
                static_evaluator_findings: record.static_evaluator_findings,
                static_evaluator_timestamp: record.static_evaluator_timestamp,
                unit_test_runner_pass: Some(record.unit_test_runner_pass),
                unit_test_runner_id: record.unit_test_runner_id,
                unit_test_runner_findings: record.unit_test_runner_findings,
                unit_test_runner_timestamp: record.unit_test_runner_timestamp,
                sealed_evaluator_pass: Some(record.sealed_evaluator_pass),
                sealed_evaluator_id: record.sealed_evaluator_id,
                sealed_evaluator_findings: record.sealed_evaluator_findings,
                sealed_evaluator_timestamp: record.sealed_evaluator_timestamp,
                promotion_gate_version: record.promotion_gate_version,
            },
            None => {
                return serde_json::to_string(&serde_json::json!({
                    "artifact_id": args.artifact_id,
                    "error": "No promotion record found for this artifact"
                }))
                .map_err(Into::into)
            }
        };

        serde_json::to_string(&response).map_err(Into::into)
    }
}
