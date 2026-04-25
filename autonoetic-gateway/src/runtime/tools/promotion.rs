use crate::causal_chain::CausalLogger;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::promotion_store::PromotionStore;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::promotion::{
    PromotionQueryArgs, PromotionQueryResponse, PromotionRecordArgs, PromotionRecordResponse,
};
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(PromotionRecordTool));
    registry.register(Box::new(PromotionQueryTool));
}

fn is_promotion_agent(manifest: &AgentManifest) -> bool {
    matches!(
        manifest.agent.id.as_str(),
        "evaluator.default" | "auditor.default"
    )
}

pub struct PromotionRecordTool;

impl NativeTool for PromotionRecordTool {
    fn name(&self) -> &'static str {
        "promotion.record"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Records promotion status (evaluator or auditor validation result) for an artifact. Only evaluator.default and auditor.default agents can call this tool.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ID being promoted (e.g., 'art_a1b2c3d4')"
                    },
                    "artifact_digest": {
                        "type": "string",
                        "description": "Optional SHA-256 digest of the artifact for integrity verification"
                    },
                    "role": {
                        "type": "string",
                        "description": "Role recording this promotion: 'evaluator' or 'auditor'",
                        "enum": ["evaluator", "auditor"]
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
                "required": ["artifact_id", "role", "pass"],
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
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: PromotionRecordArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(
            args.artifact_id.starts_with("art_"),
            "artifact_id must start with 'art_'"
        );
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
            anyhow::bail!(
                "Findings schema validation failed:\n  - {}",
                findings_with_errors.join("\n  - ")
            );
        }

        let has_error_or_critical = args.findings.iter().any(|f| {
            matches!(
                f.severity,
                autonoetic_types::promotion::FindingSeverity::Error
                    | autonoetic_types::promotion::FindingSeverity::Critical
            )
        });

        if args.pass && has_error_or_critical {
            anyhow::bail!(
                "Cannot set pass=true when findings contain 'error' or 'critical' severity. \
                 Fix the issues and re-evaluate, or set pass=false."
            );
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
                        format!("{}...", &desc[..80])
                    } else {
                        desc.clone()
                    }
                })
                .collect();
            if !warnings_without_evidence.is_empty() {
                anyhow::bail!(
                    "Cannot set pass=true when warning findings lack evidence. \
                     The following warnings need concrete evidence (e.g., sandbox output, test results) \
                     to prove the issue was investigated:\n  - {}",
                    warnings_without_evidence.join("\n  - ")
                );
            }
        }

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Promotion store requires gateway directory to be configured");
        };

        let store = PromotionStore::new(gw_dir)?;

        let record = store.record_promotion(
            args.artifact_id.clone(),
            args.artifact_digest.clone(),
            None,
            args.role.clone(),
            &manifest.agent.id,
            args.pass,
            args.findings.clone(),
            args.summary.clone(),
        )?;

        let causal_log_path = gw_dir.join("history").join("causal_chain.jsonl");
        if let Some(parent) = causal_log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(logger) = CausalLogger::new(&causal_log_path) {
            let _ = logger.log(
                &manifest.agent.id,
                session_id.unwrap_or("unknown"),
                turn_id,
                0,
                "tool",
                "promotion.record",
                EntryStatus::Success,
                Some(serde_json::json!({
                    "arguments": {
                        "artifact_id": args.artifact_id,
                        "role": args.role.as_str(),
                        "pass": args.pass,
                    }
                })),
            );
        }

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
        "promotion.query"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Queries the promotion status of an artifact. Returns evaluator and auditor validation results, or null if no promotion record exists.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ID to query promotion status for (e.g., 'art_a1b2c3d4')"
                    }
                },
                "required": ["artifact_id"],
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

        anyhow::ensure!(
            args.artifact_id.starts_with("art_"),
            "artifact_id must start with 'art_'"
        );

        let Some(gw_dir) = gateway_dir else {
            anyhow::bail!("Promotion store requires gateway directory to be configured");
        };

        let store = PromotionStore::new(gw_dir)?;

        let response = match store.get_promotion(&args.artifact_id) {
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
