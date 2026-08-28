use crate::causal_chain::CausalLogger;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::promotion_store::PromotionStore;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
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

/// Manifest explicitly lists a native tool id under [`Capability::SandboxFunctions`]
/// (same prefix rules as [`PolicyEngine::can_invoke_tool`]).
pub fn manifest_sandbox_allows_tool(manifest: &AgentManifest, tool_name: &str) -> bool {
    manifest.capabilities.iter().any(|cap| {
        if let Capability::SandboxFunctions { allowed } = cap {
            allowed.iter().any(|pattern| {
                let prefix = pattern.trim_end_matches('*');
                tool_name.starts_with(prefix)
            })
        } else {
            false
        }
    })
}

fn manifest_has_broad_artifact_exec_cap(manifest: &AgentManifest) -> bool {
    manifest.capabilities.iter().any(|cap| {
        matches!(
            cap,
            Capability::ArtifactExecution | Capability::Evaluation { .. }
        )
    })
}

/// Federation exec gates may run artifact entrypoints via [`artifact_exec`] without
/// declaring broad `ArtifactExecution` or `Evaluation` capabilities.
///
/// Declared in SKILL frontmatter: list `artifact_exec` and `promotion_` under
/// `SandboxFunctions.allowed`. Static reviewers keep `promotion_` only (no exec).
/// Agents with `CodeExecution` / `Evaluation` use the standard exec gates instead.
pub fn manifest_may_exec_artifact_in_promotion_gate(manifest: &AgentManifest) -> bool {
    !manifest_has_broad_artifact_exec_cap(manifest)
        && manifest_sandbox_allows_tool(manifest, "artifact_exec")
        && manifest_sandbox_allows_tool(manifest, "promotion_record")
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
                    "execution_trace_id": {
                        "type": "string",
                        "description": "UUID returned as execution_trace_id on the artifact_exec or sandbox_exec result for this run. Required for unit_test_runner, static_evaluator, sealed_evaluator, and evaluator roles. pass is derived from exit_code=0."
                    },
                    "pass": {
                        "type": "boolean",
                        "description": "Whether this validation passed. Required for auditor only; ignored for execution roles (pass is trace-derived)."
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
                "required": ["role"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        is_promotion_agent(manifest)
    }

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        use crate::runtime::guidance::{
            GuidanceBlock, GuidanceCondition, PHASE_ARTIFACT_BUILT, PHASE_GATED_PRIORITY_FLOOR,
        };
        // Centralized from static_evaluator/sealed_evaluator/auditor SKILL.md
        // (#466): the call protocol is uniform across promotion agents.
        // Role-specific exceptions (e.g. unit_test_runner's "no tests → don't
        // call", sealed_evaluator's "defer until approval resolves") stay in
        // those manifests.
        vec![GuidanceBlock {
            id: "promotion.record_protocol",
            // A verdict is recorded *on an artifact*, so the procedure cannot
            // apply before one exists (RFC P2). Gate agents that never build
            // out of paying for it.
            when: GuidanceCondition::All(vec![
                GuidanceCondition::ToolPresent("promotion_record"),
                GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
            ]),
            priority: PHASE_GATED_PRIORITY_FLOOR,
            prose: "**Recording your verdict.** When your evaluation/audit reaches a verdict, call \
`promotion_record` with the `artifact_ref` you reviewed. Execution roles (`unit_test_runner`, \
`sealed_evaluator`) must attach `execution_trace_id` from the run — copy the UUID from the \
`artifact_exec` / `sandbox_exec` tool result (`execution_trace_id` field); the gateway \
derives `pass` from `exit_code=0`; do not declare success without a trace. The `auditor` and \
`static_evaluator` roles set `pass` explicitly; only `critical` findings can veto an otherwise-passing \
audit. Include `findings` and `summary` as advisory annotation. Use those exact field names — not \
alternates like `outcome`. (Your role may define cases where the gate is inapplicable and you should NOT \
call this — e.g. no tests found; follow that role-specific guidance.)"
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

        // NOTE(optimized-re-federation): `artifact_canonical_digest` covers the
        // whole bundle, so a rebuild that touches only SKILL.md produces a new
        // digest and voids every prior promotion_record — forcing the planner
        // to re-run unit_test_runner, static_evaluator, AND auditor even when
        // the code two of those gates reviewed did not change (see
        // session-964ea6d7 for the three-round case). A future optimization:
        // key each role's record on a per-role input digest (e.g. code files
        // only for unit_test_runner/auditor, manifest for static_evaluator) so
        // unchanged-role verdicts survive a manifest-only rebuild. This cuts
        // across the tamper-resistance story of content-addressed records, so
        // it is tracked as follow-up rather than changed here.

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

        let (pass, execution_trace_id) =
            if crate::runtime::promotion_evidence::role_requires_execution_trace(&args.role) {
                let Some(trace_id) = args
                    .execution_trace_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    return Ok(
                        ToolError::validation(
                            format!(
                                "role '{}' requires execution_trace_id from a completed run",
                                args.role.as_str()
                            ),
                            None::<String>,
                        )
                        .with_code("missing_execution_evidence")
                        .with_repair_hint(
                            "Run the evaluation in sandbox/artifact_exec, then attach the execution_trace_id UUID from that tool result.",
                        )
                        .to_error_response(),
                    );
                };
                let Some(gs) = gateway_store.as_ref() else {
                    return Ok(ToolError::resource(
                        "GatewayStore required to verify execution_trace_id",
                        None::<String>,
                    )
                    .to_error_response());
                };
                let Some(trace) = gs.get_execution_trace(trace_id)? else {
                    return Ok(ToolError::validation(
                        format!("execution trace '{}' not found", trace_id),
                        None::<String>,
                    )
                    .with_code("execution_trace_not_found")
                    .with_repair_hint(
                        "Copy the execution_trace_id UUID from your artifact_exec or sandbox_exec \
                         tool result. Do not use artifact_ref, session_id, file paths, or digests.",
                    )
                    .to_error_response());
                };
                let pass = crate::runtime::promotion_evidence::trace_indicates_pass(&trace);
                (pass, Some(trace_id.to_string()))
            } else if matches!(args.role, autonoetic_types::promotion::PromotionRole::Auditor) {
                let mut pass = args.pass.unwrap_or(false);
                if crate::runtime::promotion_evidence::auditor_critical_veto(&args.findings) {
                    pass = false;
                }
                (pass, None)
            } else if matches!(args.role, autonoetic_types::promotion::PromotionRole::StaticEvaluator) {
                let mut pass = args.pass.unwrap_or(false);
                if crate::runtime::promotion_evidence::findings_block_explicit_pass(&args.findings) {
                    pass = false;
                }
                (pass, None)
            } else {
                return Ok(ToolError::validation(
                    format!("unsupported promotion role '{}'", args.role.as_str()),
                    None::<String>,
                )
                .to_error_response());
            };

        let causal_log_path = gw_dir.join("history").join("causal_chain.jsonl");
        if let Some(parent) = causal_log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let trace_id_for_log = execution_trace_id.clone();

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
                        "pass": pass,
                        "execution_trace_id": trace_id_for_log,
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
            pass,
            args.findings.clone(),
            args.summary.clone(),
            execution_trace_id,
        )?;

        // Federation carry-forward (Stage 1): bind this verdict to the exact
        // bytes the gate reviewed by copying the artifact's current per-input
        // digests onto the record. Non-blocking — a failure here leaves the
        // digests as None (= unverifiable = must re-run under carry-forward),
        // which is the fail-closed posture, never a promotion failure.
        // Strictness is still `off` in Stage 1, so this is pure provenance.
        if let Some(artifact_store) = crate::artifact_store::ArtifactStore::new(gw_dir).ok() {
            if let Ok(bundle) = artifact_store.inspect(&artifact_id) {
                let digests = crate::runtime::federation_carry_forward::compute_federation_digests(
                    &bundle, &artifact_store,
                );
                if let Err(e) = store.set_federation_digests(
                    &artifact_id,
                    digests.code_digest,
                    digests.contract_digest,
                    digests.prose_digest,
                ) {
                    tracing::warn!(
                        target: "promotion",
                        artifact_id = %artifact_id,
                        error = %e,
                        "failed to attach federation carry-forward digests (non-blocking)"
                    );
                }
            } else {
                tracing::debug!(
                    target: "promotion",
                    artifact_id = %artifact_id,
                    "artifact not resolvable for federation digest attachment (non-agent-bundle or missing); skipping"
                );
            }
        }

        // Bless-on-promotion (determinism inc 3): on a passing verdict, freeze
        // the resolved dependency closure the validated run used. Best-effort
        // provenance recorded *after* the gate decision — it never alters the
        // gate and never fails the promotion. Idempotent across role verdicts
        // (the artifact's layers don't change between them).
        if pass {
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
            "pass": pass,
            "execution_trace_id": trace_id_for_log,
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
                    // Federation carry-forward digests (Stage 1). `null` for
                    // records predating this feature (= unverifiable under
                    // carry-forward) or non-agent-bundle artifacts. Strictness
                    // is still `off` in Stage 1, so these are pure provenance.
                    "code_digest": record.code_digest,
                    "contract_digest": record.contract_digest,
                    "prose_digest": record.prose_digest,
                    // Federation carry-forward provenance (Stage 4). Empty
                    // object when every verdict on this artifact was freshly
                    // run. Each entry: which prior artifact + role the verdict
                    // was carried from, the verified digests, justification,
                    // and the strictness in effect when the carry was accepted.
                    // The operator uses this to tell a carried verdict apart
                    // from a freshly-run one.
                    "carried_verdicts": record.carried_roles,
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
        use crate::runtime::guidance::{SessionPhase, PHASE_ARTIFACT_BUILT};

        let blocks = PromotionRecordTool.guidance();
        let tools = vec!["promotion_record".to_string()];

        // A verdict is recorded *on an artifact*, so the procedure is
        // phase-gated (RFC P2): holding the tool is no longer enough.
        let pre = GuidanceContext { active_tool_names: &tools, ..Default::default() };
        assert!(
            compose_guidance(&blocks, &pre).is_empty(),
            "verdict protocol must not load before an artifact exists"
        );

        let mut phase = SessionPhase::default();
        phase.insert(PHASE_ARTIFACT_BUILT);
        let ctx = GuidanceContext {
            active_tool_names: &tools,
            phase: Some(&phase),
            ..Default::default()
        };
        // Phase-gated blocks render in the tail, not the standing section.
        let out = compose_guidance(&blocks, &ctx).phase_tail;
        assert!(out.contains("Recording your verdict"), "block text missing: {out}");
        assert!(out.contains("not alternates like `outcome`"));

        // Absent when promotion_record isn't advertised, even once the phase is
        // reached — phase narrows, it never widens reach.
        let no_tool = GuidanceContext { phase: Some(&phase), ..Default::default() };
        assert!(compose_guidance(&blocks, &no_tool).is_empty());
    }
}

#[cfg(test)]
mod promotion_gate_exec_tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn base_manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            remote_access: None,
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                mounts: Vec::new(),
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: agent_id.to_string(),
                name: agent_id.to_string(),
                description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities,
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            adapter: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
        }
    }

    fn promotion_exec_sandbox() -> Capability {
        Capability::SandboxFunctions {
            allowed: vec![
                "artifact_inspect".to_string(),
                "artifact_exec".to_string(),
                "promotion_".to_string(),
            ],
        }
    }

    #[test]
    fn promotion_gate_exec_any_agent_id_with_declared_tools() {
        let manifest = base_manifest(
            "acme.custom_unit_test_runner",
            vec![
                promotion_exec_sandbox(),
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string()],
                },
            ],
        );
        assert!(manifest_may_exec_artifact_in_promotion_gate(&manifest));
    }

    #[test]
    fn static_evaluator_promotion_only_not_exec_gate() {
        let manifest = base_manifest(
            "static_evaluator.default",
            vec![
                Capability::SandboxFunctions {
                    allowed: vec!["knowledge_".to_string(), "promotion_".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string()],
                },
            ],
        );
        assert!(!manifest_may_exec_artifact_in_promotion_gate(&manifest));
    }

    #[test]
    fn artifact_execution_agent_uses_standard_exec_path() {
        let manifest = base_manifest(
            "sealed_evaluator.default",
            vec![
                promotion_exec_sandbox(),
                Capability::ArtifactExecution,
            ],
        );
        assert!(!manifest_may_exec_artifact_in_promotion_gate(&manifest));
    }
}
