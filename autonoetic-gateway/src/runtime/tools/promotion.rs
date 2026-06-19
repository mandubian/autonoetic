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
                        "description": "Artifact ref to record findings against. Prefer using artifact_ref (ar.*) instead."
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

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
        // Centralized from static_evaluator/sealed_evaluator/auditor SKILL.md
        // (#466): the call protocol is uniform across promotion agents.
        // Role-specific exceptions (e.g. unit_test_runner's "no tests → don't
        // call", sealed_evaluator's "defer until approval resolves") stay in
        // those manifests.
        vec![GuidanceBlock {
            id: "promotion.record_protocol",
            when: GuidanceCondition::ToolPresent("promotion_record"),
            priority: 10,
            prose: "**Recording your verdict.** When your evaluation/audit reaches a verdict, call \
`promotion_record` with the `artifact_ref` you reviewed. Only `role` and `pass` are required; include \
`findings` and `summary` too. Use those exact field names — not alternates like `outcome`. `pass` is \
the boolean equivalent of your verdict (`evaluator_pass` / `auditor_pass`); record a failing verdict \
as well, with `pass: false`. (Your role may define cases where the gate is inapplicable and you should \
NOT call this — e.g. no tests found; follow that role-specific guidance.)"
                .to_string(),
        }]
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

        anyhow::ensure!(
            args.content_digest.is_none(),
            "content_digest is gateway-owned and must not be provided to promotion.record"
        );

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Promotion store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        // Resolve artifact_id from artifact_ref if needed.
        // Capture artifact_canonical_digest and user-facing ref for the response.
        let (artifact_id, artifact_canonical_digest, user_ref) = match (&args.artifact_id, &args.artifact_ref) {
            (Some(id), _) if id.starts_with("art_") => {
                let digest = crate::artifact_store::ArtifactStore::new(gw_dir)
                    .ok()
                    .and_then(|s| s.inspect(id).ok())
                    .map(|b| b.artifact_canonical_digest);
                (id.clone(), digest, None::<String>)
            }
            (_, Some(ref_id)) if ref_id.starts_with("art_") => {
                let digest = crate::artifact_store::ArtifactStore::new(gw_dir)
                    .ok()
                    .and_then(|s| s.inspect(ref_id).ok())
                    .map(|b| b.artifact_canonical_digest);
                (ref_id.clone(), digest, None::<String>)
            }
            (_, Some(ref_id)) if ref_id.starts_with("art_") => {
                let store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
                let bundle = store.inspect(ref_id)?;
                (ref_id.clone(), Some(bundle.artifact_canonical_digest), None::<String>)
            }
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
                (record.artifact_id, Some(record.artifact_canonical_digest), Some(ref_id.to_string()))
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

        let mut record = store.record_promotion(
            artifact_id.clone(),
            args.artifact_digest.clone(),
            None,
            args.role.clone(),
            &manifest.agent.id,
            args.pass,
            args.findings.clone(),
            args.summary.clone(),
        )?;

        // Bless-on-promotion (determinism inc 3): on a passing verdict, freeze
        // the resolved dependency closure the validated run used. Best-effort
        // provenance recorded *after* the gate decision — it never alters the
        // gate and never fails the promotion. Idempotent across role verdicts
        // (the artifact's layers don't change between them).
        if args.pass {
            match bless_resolved_closure(gw_dir, &artifact_id, &store) {
                Ok(true) => {
                    // Re-read so the response reflects the freshly-blessed set.
                    if let Some(updated) = store.get_promotion(&artifact_id) {
                        record = updated;
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "promotion",
                    artifact_id = %artifact_id,
                    error = %e,
                    "failed to bless resolved dependency closure (non-blocking)"
                ),
            }
        }

        let response = serde_json::json!({
            "ok": true,
            "promotion_record": {
                "content_digest": record.content_digest,
                "artifact_digest": record.artifact_digest,
                "artifact_canonical_digest": artifact_canonical_digest,
                "artifact_ref": user_ref,
                "evaluator_pass": record.evaluator_pass,
                "auditor_pass": record.auditor_pass,
                "evaluator_id": record.evaluator_id,
                "auditor_id": record.auditor_id,
                "evaluator_findings": record.evaluator_findings,
                "auditor_findings": record.auditor_findings,
                "evaluator_timestamp": record.evaluator_timestamp,
                "auditor_timestamp": record.auditor_timestamp,
                "static_evaluator_pass": record.static_evaluator_pass,
                "static_evaluator_id": record.static_evaluator_id,
                "static_evaluator_findings": record.static_evaluator_findings,
                "static_evaluator_timestamp": record.static_evaluator_timestamp,
                "unit_test_runner_pass": record.unit_test_runner_pass,
                "unit_test_runner_id": record.unit_test_runner_id,
                "unit_test_runner_findings": record.unit_test_runner_findings,
                "unit_test_runner_timestamp": record.unit_test_runner_timestamp,
                "sealed_evaluator_pass": record.sealed_evaluator_pass,
                "sealed_evaluator_id": record.sealed_evaluator_id,
                "sealed_evaluator_findings": record.sealed_evaluator_findings,
                "sealed_evaluator_timestamp": record.sealed_evaluator_timestamp,
                "promotion_gate_version": record.promotion_gate_version,
                "blessed_packages": record.blessed_packages,
            }
        });

        serde_json::to_string(&response).map_err(Into::into)
    }
}

/// Freeze the resolved dependency closure for a promoted artifact: aggregate the
/// resolved-version provenance across the artifact's layers and bless it onto the
/// promotion record. Returns `Ok(true)` if a non-empty closure was blessed,
/// `Ok(false)` when the artifact has no dependency layers / provenance. Pure
/// provenance — callers treat errors as non-blocking.
fn bless_resolved_closure(
    gw_dir: &Path,
    artifact_id: &str,
    store: &PromotionStore,
) -> anyhow::Result<bool> {
    let bundle = crate::artifact_store::ArtifactStore::new(gw_dir)?.inspect(artifact_id)?;
    let layer_ids: Vec<String> = bundle.layers.iter().map(|l| l.layer_id.clone()).collect();
    if layer_ids.is_empty() {
        return Ok(false);
    }
    let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
    let blessed = layer_store.aggregate_resolved_packages(&layer_ids);
    if blessed.is_empty() {
        return Ok(false);
    }
    store.set_blessed_packages(artifact_id, blessed)
}

pub struct PromotionQueryTool;

impl NativeTool for PromotionQueryTool {
    fn name(&self) -> &'static str {
        "promotion_query"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Queries the promotion status of an artifact. Returns all role validation results (evaluator, auditor, static_evaluator, unit_test_runner, sealed_evaluator). Prefer using `artifact_ref` (short `ar.*` form) — it is the scoped agent-facing handle. The `artifact_id` field is also accepted for compatibility.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "Artifact ref. Prefer using artifact_ref (ar.*) instead."
                    },
                    "artifact_ref": {
                        "type": "string",
                        "description": "Short artifact ref (e.g., 'ar.386f5b222421'). Resolved server-side to the canonical artifact_id. Alternative input form to artifact_id — pass exactly one of the two."
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

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource("Promotion store requires gateway directory to be configured", None::<String>).to_error_response());
        };

        let (artifact_id, artifact_canonical_digest, user_ref) = match (args.artifact_id.as_deref(), args.artifact_ref.as_deref()) {
            (Some(id), _) if id.starts_with("art_") => {
                let digest = crate::artifact_store::ArtifactStore::new(gw_dir)
                    .ok()
                    .and_then(|s| s.inspect(id).ok())
                    .map(|b| b.artifact_canonical_digest);
                (id.to_string(), digest, None::<String>)
            }
            (Some(id), _) => anyhow::bail!(
                "promotion_query: artifact_id must start with 'art_' (got '{}'). \
                 If you have a short ref like 'ar.X', pass it as 'artifact_ref' instead.",
                id
            ),
            (None, Some(ref_id)) if ref_id.starts_with("art_") => {
                let digest = crate::artifact_store::ArtifactStore::new(gw_dir)
                    .ok()
                    .and_then(|s| s.inspect(ref_id).ok())
                    .map(|b| b.artifact_canonical_digest);
                (ref_id.to_string(), digest, None::<String>)
            }
            (None, Some(ref_id)) => {
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
                (record.artifact_id, Some(record.artifact_canonical_digest), Some(ref_id.to_string()))
            }
            (None, None) => anyhow::bail!(
                "promotion_query: provide either 'artifact_id' (e.g. 'art_a1b2c3d4') \
                 or 'artifact_ref' (e.g. 'ar.386f5b222421'). Both are alternatives — \
                 pass one, not neither."
            ),
        };

        let store = PromotionStore::new(gw_dir)?;

        let waived_validations: Vec<serde_json::Value> =
            if let Some(ref gs) = _gateway_store {
                match gs.list_waivers_for_artifact(&artifact_id) {
                    Ok(waivers) => waivers
                        .into_iter()
                        .map(|w| {
                            serde_json::json!({
                                "waiver_id": w.waiver_id,
                                "validation_id": w.validation_id,
                                "validation_class": w.validation_class.as_str(),
                                "waived_by": w.waived_by,
                                "reason": w.reason,
                                "created_at": w.created_at,
                            })
                        })
                        .collect(),
                    Err(e) => {
                        tracing::warn!(target: "promotion", artifact_id = %artifact_id, error = %e, "failed to load waivers");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

        match store.get_promotion(&artifact_id) {
            Some(record) => {
                let response = serde_json::json!({
                    "artifact_canonical_digest": artifact_canonical_digest,
                    "artifact_ref": user_ref,
                    "content_digest": record.content_digest,
                    "waived_validations": waived_validations,
                    "evaluator_pass": record.evaluator_pass,
                    "auditor_pass": record.auditor_pass,
                    "evaluator_id": record.evaluator_id,
                    "auditor_id": record.auditor_id,
                    "evaluator_findings": record.evaluator_findings,
                    "auditor_findings": record.auditor_findings,
                    "evaluator_timestamp": record.evaluator_timestamp,
                    "auditor_timestamp": record.auditor_timestamp,
                    "static_evaluator_pass": record.static_evaluator_pass,
                    "static_evaluator_id": record.static_evaluator_id,
                    "static_evaluator_findings": record.static_evaluator_findings,
                    "static_evaluator_timestamp": record.static_evaluator_timestamp,
                    "unit_test_runner_pass": record.unit_test_runner_pass,
                    "unit_test_runner_id": record.unit_test_runner_id,
                    "unit_test_runner_findings": record.unit_test_runner_findings,
                    "unit_test_runner_timestamp": record.unit_test_runner_timestamp,
                    "sealed_evaluator_pass": record.sealed_evaluator_pass,
                    "sealed_evaluator_id": record.sealed_evaluator_id,
                    "sealed_evaluator_findings": record.sealed_evaluator_findings,
                    "sealed_evaluator_timestamp": record.sealed_evaluator_timestamp,
                    "promotion_gate_version": record.promotion_gate_version,
                });
                serde_json::to_string(&response).map_err(Into::into)
            }
            None => {
                Ok(ToolError::not_found("Promotion record", Some("Ensure a promotion record exists for this artifact before querying."))
                    .with_code("promotion_record_not_found")
                    .to_error_response())
            }
        }
    }
}

#[cfg(test)]
mod guidance_tests {
    use super::*;
    use crate::runtime::guidance::{compose_guidance, GuidanceContext};

    #[test]
    fn promotion_record_contributes_verdict_block() {
        let blocks = PromotionRecordTool.guidance();
        let tools = vec!["promotion_record".to_string()];
        let ctx = GuidanceContext { active_tool_names: &tools, ..Default::default() };
        let out = compose_guidance(&blocks, &ctx);
        assert!(out.contains("Recording your verdict"), "block text missing: {out}");
        assert!(out.contains("not alternates like `outcome`"));

        // Absent when promotion_record isn't in the advertised tool set.
        assert_eq!(compose_guidance(&blocks, &GuidanceContext::default()), "");
    }
}
