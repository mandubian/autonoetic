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
    #[serde(default)]
    revision_id: Option<String>,
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
                "required": ["agent_id", "role_verdicts", "planner_synthesis", "root_session_id"],
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
                        "description": "The seeded revision being proposed for promotion. \
                            Accepted forms: the FULL id 'rev_sha256:<hex>' OR the SHORT id \
                            'rev_<short>' / bare '<short>' (the short form from \
                            agent_revision_create's short_ref, e.g. \
                            'planner.default@rev_abc12345' → pass 'rev_abc12345'). \
                            OMIT this field entirely for a NEW agent whose artifact has not \
                            been seeded into a revision yet: the review then binds to the \
                            artifact (pass artifact_ref) and capabilities are read from the \
                            artifact's SKILL.md. Do not invent placeholder ids like \
                            'rev-initial'."
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
        // Callers also put `ar.*` refs in artifact_id — resolve those the same way.
        let sid = _session_id.unwrap_or("");
        let caller_artifact_id = if let Some(ref ref_id) = args.artifact_ref {
            store
                .resolve_artifact_ref_any_scope(ref_id, sid)?
                .map(|r| r.artifact_id)
                .unwrap_or_else(|| args.artifact_id.clone())
        } else if args.artifact_id.starts_with("ar.") {
            store
                .resolve_artifact_ref_any_scope(&args.artifact_id, sid)?
                .map(|r| r.artifact_id)
                .unwrap_or_else(|| args.artifact_id.clone())
        } else {
            args.artifact_id.clone()
        };

        // Resolve the proposed revision when one was given: accept the full id
        // (rev_sha256:...) or a short id via the short_id_index.
        //
        // Short ids are presented to LLMs as `agent@rev_<short>` (see
        // agent_revision_create / revision_list responses), so callers
        // naturally pass back either the bare short token (`abc12345`) or the
        // prefixed form (`rev_abc12345`). The short_id_index stores the BARE
        // token only, so strip a leading `rev_` before lookup — same rule as
        // AgentRepository::resolve_agent (`repository.rs`). Without this, an
        // LLM passing back the very `rev_` form we showed it would be rejected
        // as an unknown revision.
        let resolved_revision = match args.revision_id.as_deref() {
            Some(rid) => match store.get_agent_revision(rid)? {
                Some(rev) => Some(rev),
                None => {
                    let short_lookup = rid
                        .strip_prefix("rev_")
                        .filter(|s| !s.is_empty())
                        .unwrap_or(rid);
                    match store.lookup_short_id(short_lookup)? {
                        Some(full_id) => store.get_agent_revision(&full_id)?,
                        None => {
                            // A revision id that resolves to nothing is a caller
                            // error (typo, stale id, or an invented placeholder) —
                            // reviewing artifact contents while the approval names
                            // a phantom revision would let the two diverge.
                            return Ok(autonoetic_types::tool_error::ToolError::validation(
                                format!(
                                    "revision '{}' does not exist for agent '{}' (not a known \
                                     full 'rev_sha256:...' or short revision id 'rev_...'). For \
                                     an existing agent, create the revision first \
                                     (agent_revision_create with the artifact_ref) and \
                                     re-escalate with the returned id. For a NEW agent whose \
                                     artifact is not seeded yet, OMIT revision_id and pass \
                                     artifact_ref — the review binds to the artifact.",
                                    rid, args.agent_id
                                ),
                                None::<String>,
                            )
                            .to_error_response());
                        }
                    }
                }
            },
            None => None,
        };

        let (canonical_artifact_id, canonical_revision_id, revision_seeded) =
            match resolved_revision {
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
                    (art, rev.revision_id.clone(), true)
                }
                None => {
                    // No revision: new-agent escalate-before-install. The promote
                    // gate binds the approved escalation by artifact, so the
                    // review proceeds from the artifact alone under a derived
                    // internal key (never a caller-invented placeholder).
                    if store.resolve_alias(&args.agent_id)?.is_some() {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            format!(
                                "agent '{}' is already installed — escalation requires the \
                                 seeded revision being promoted. Create it first \
                                 (agent_revision_create with the artifact_ref), then \
                                 re-escalate with the returned 'rev_sha256:...' id.",
                                args.agent_id
                            ),
                            None::<String>,
                        )
                        .to_error_response());
                    }
                    if caller_artifact_id.is_empty() {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            format!(
                                "no revision_id and no artifact for agent '{}' — the review \
                                 has nothing to bind to. Pass the artifact_ref (ar.*) \
                                 returned by artifact_build, or seed the revision first \
                                 (agent_revision_create) and escalate its 'rev_sha256:...' id.",
                                args.agent_id
                            ),
                            None::<String>,
                        )
                        .to_error_response());
                    }
                    let derived = format!("unseeded:{}", caller_artifact_id);
                    (caller_artifact_id.clone(), derived, false)
                }
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

        // #738: when the promotion also broadens capabilities (or is a new
        // cap-bearing agent), mint ONE merged `RevisionPromote` approval that
        // carries both the federation jury context and the capability delta,
        // under Critical hardening (5s dwell + confirm phrase + capability ack).
        // The operator decides once; `agent_revision_promote` then accepts this
        // single approval as covering BOTH the R++2 capability gate and the
        // FullJury review gate. When there is no capability delta (e.g. a new
        // zero-cap agent, or no broadening), there is nothing to ack — the
        // federation review stands alone as a `SessionEscalate{PromotionReview}`
        // and the operator only approves the jury verdicts.
        let capability_delta = if let (Some(gw_dir), Some(cfg)) = (gateway_dir, _config) {
            // Seeded revision: read the declared capabilities from the revision
            // directory. Unseeded (new-agent escalate-before-install): read them
            // from the artifact's SKILL.md — the same artifact the approved
            // escalation will bind to at promote time.
            let loaded_caps = if revision_seeded {
                super::agent_revision::load_revision_capabilities(
                    gw_dir,
                    &args.agent_id,
                    &canonical_revision_id,
                )
            } else {
                tracing::info!(
                    target: "federation",
                    agent_id = %args.agent_id,
                    revision_id = %canonical_revision_id,
                    artifact_id = %canonical_artifact_id,
                    "federation.escalate: revision not seeded yet (new agent) — \
                     reading declared capabilities from the artifact SKILL.md"
                );
                super::agent_revision::load_artifact_capabilities(
                    gw_dir,
                    &canonical_artifact_id,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "revision '{}' is not seeded and the artifact's SKILL.md is unreadable: {}. \
                         The escalation for a new agent binds to the artifact, so its SKILL.md \
                         must be readable — pass the artifact_ref (ar.*) from artifact_build, \
                         or seed the revision first (agent_revision_create) and re-escalate \
                         with its 'rev_sha256:...' id",
                        canonical_revision_id, e
                    )
                })
            };
            match loaded_caps {
                Ok(current_caps) => {
                    match super::agent_revision::check_capability_delta(
                        &store,
                        gw_dir,
                        &args.agent_id,
                        &canonical_revision_id,
                        &current_caps,
                        cfg.capability_delta_gate_mode,
                        cfg.require_operator_approval_for_new_agents,
                    ) {
                        Ok(delta) => delta,
                        Err(e) => {
                            // Fail closed (#746 review): a delta-computation
                            // failure must not downgrade the review to jury-only
                            // — that would let a cap-bearing new agent bypass
                            // R++2 via new_agent_approved_via_escalation, and
                            // reintroduce the double decision for existing
                            // agents. Surface the error; escalate again once
                            // the underlying issue is fixed.
                            return Ok(autonoetic_types::tool_error::ToolError::execution(
                                format!(
                                    "capability delta computation failed for '{}' rev '{}': {}. \
                                     Refusing to downgrade the promotion review to jury-only \
                                     (R++2 fail-closed); fix the underlying error and re-escalate.",
                                    args.agent_id, canonical_revision_id, e
                                ),
                                None::<String>,
                            )
                            .to_error_response());
                        }
                    }
                }
                Err(e) => {
                    // Fail closed (#746 review): same rationale as above — an
                    // unreadable capability set must not weaken the gate.
                    return Ok(autonoetic_types::tool_error::ToolError::execution(
                        format!(
                            "could not load declared capabilities for '{}' rev '{}': {}. \
                             Refusing to downgrade the promotion review to jury-only \
                             (R++2 fail-closed); fix the underlying error and re-escalate.",
                            args.agent_id, canonical_revision_id, e
                        ),
                        None::<String>,
                    )
                    .to_error_response());
                }
            }
        } else {
            None
        };

        // The federation jury context embedded in the merged action so the
        // operator's single decision surfaces both the delta and the verdicts,
        // and the FullJury gate can bind by content digest (#653 structural fix).
        let federation_context = Some(autonoetic_types::background::RevisionPromoteFederationContext {
            artifact_id: canonical_artifact_id.clone(),
            content_digest: escalation.artifact_digest.clone(),
            role_verdicts_summary: summarize_role_verdicts(&escalation.role_verdicts),
            planner_synthesis: args.planner_synthesis.clone(),
        });

        let gate_service = GateService::new(store.clone());

        // Branch on whether this promotion carries a capability delta. The
        // merged `RevisionPromote` path is the #738 single-decision flow; the
        // `SessionEscalate` path is the legacy jury-only review (no caps to ack).
        let gate_req = if let Some(delta) = capability_delta.as_ref() {
            // Merged single-decision path — RevisionPromote with federation context.
            let outgoing_revision_id = super::agent_revision::outgoing_revision_id(
                &store,
                &args.agent_id,
            )?
            .unwrap_or_default();
            let added_capabilities: Vec<String> = delta.added.clone();
            let broadened_capabilities: Vec<String> = delta
                .broadened
                .iter()
                .map(|b| b.capability_type.clone())
                .collect();
            let action = ScheduledAction::RevisionPromote {
                agent_id: args.agent_id.clone(),
                revision_id: canonical_revision_id.clone(),
                outgoing_revision_id: outgoing_revision_id.clone(),
                added_capabilities: added_capabilities.clone(),
                broadened_capabilities: broadened_capabilities.clone(),
                payload: Some(serde_json::json!({
                    "escalation_id": escalation_id,
                    "artifact_id": canonical_artifact_id,
                    "revision_id": canonical_revision_id,
                    "federation_review": true,
                })),
                federation_context: federation_context.clone(),
            };
            GateRequest {
                kind: GateKind::Approval {
                    action: action.clone(),
                    // ExactPayload: the merged action carries the delta + jury
                    // context, so retries dedup onto the genuinely identical
                    // pending gate. Critical hardening (dwell + confirm phrase)
                    // is auto-applied by classify_approval_risk for RevisionPromote.
                    targets: Vec::new(),
                    match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                },
                manifest: _manifest,
                session_id: _session_id,
                run_context: _run_context,
                config: _config,
                context: DecisionContext::tier2(
                    format!(
                        "Federated promotion of agent {} revision {} (jury + capability delta)",
                        args.agent_id, canonical_revision_id
                    ),
                    "R++2 capability acknowledgement + federation jury review (#738 single decision)",
                    format!(
                        "Added capabilities: {:?}; broadened: {:?}; artifact {} with {} role verdict(s). \
                         Approve only if you acknowledge each capability AND accept the jury verdicts.",
                        added_capabilities, broadened_capabilities, canonical_artifact_id,
                        escalation.role_verdicts.len()
                    ),
                    "Acknowledge every added/broadened capability by name and approve only if all \
                     federation role verdicts support promotion; reject if any critical role failed.",
                )
                .with_analysis(args.planner_synthesis.clone()),
                summary: format!(
                    "Federated promotion: agent '{}' revision '{}' (jury review + capability delta)",
                    args.agent_id, canonical_revision_id
                ),
                approval_ref: None,
                pre_validated: false,
                cache_backfill: None,
                request_id: None,
                turn_id: _turn_id,
            }
        } else {
            // Legacy jury-only review — no capability delta to acknowledge.
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
            GateRequest {
                kind: GateKind::Approval {
                    action: action.clone(),
                    // `targets` is intentionally empty: SessionEscalate carries no
                    // host targets. Dedup safety relies on two layered guards in
                    // find_pending_for_targets: (1) MatchStrategy::ExactPayload
                    // short-circuits via exact_payload_covers (full structural
                    // equality, including the escalation_id/artifact_id/revision_id
                    // in the payload) *before* the "empty targets → any pending of
                    // same kind" fallback; and (2) the SessionEscalate sub-type guard
                    // rejects cross-EscalationKind collisions (guidance-request vs
                    // promotion-review). Do NOT loosen the strategy or change the
                    // payload without preserving both guards, or an unrelated
                    // pending session_escalate approval could be reused and the
                    // escalation projection linked to the wrong approval row
                    // (#724 Part B review).
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
            }
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

/// One-line summary of the federation role verdicts, embedded in the merged
/// `RevisionPromote` approval so the operator sees the jury outcome alongside
/// the capability delta (#738).
fn summarize_role_verdicts(verdicts: &[RoleVerdictSummary]) -> String {
    verdicts
        .iter()
        .map(|v| {
            let role = serde_json::to_value(&v.role)
                .ok()
                .and_then(|val| val.as_str().map(String::from))
                .unwrap_or_else(|| "unknown_role".to_string());
            format!("{}: {}", role, if v.passed { "pass" } else { "fail" })
        })
        .collect::<Vec<_>>()
        .join(", ")
}
