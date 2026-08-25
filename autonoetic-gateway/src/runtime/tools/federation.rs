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

/// The `federation_escalate` *procedure*, gated on the session actually having
/// an artifact (`docs/internals/prompt/burden-study.md`).
///
/// This prose used to live in the tool's `description` and its `revision_id`
/// schema field, i.e. in every planner turn from turn 1 — including the large
/// majority of sessions that never build anything. Escalation is meaningless
/// before an artifact exists, so [`PHASE_ARTIFACT_BUILT`] is the precise moment
/// it becomes worth its tokens.
pub fn escalate_procedure_block() -> crate::runtime::guidance::GuidanceBlock {
    use crate::runtime::guidance::{
        GuidanceBlock, GuidanceCondition, PHASE_ARTIFACT_BUILT, PHASE_GATED_PRIORITY_FLOOR,
    };
    GuidanceBlock {
        id: "federation.escalate_procedure",
        // Phase-gated blocks render last so a newly-earned fact appends to the
        // prompt prefix rather than inserting into it.
        priority: PHASE_GATED_PRIORITY_FLOOR,
        when: GuidanceCondition::All(vec![
            GuidanceCondition::ToolPresent("federation_escalate"),
            GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
        ]),
        prose: "**Escalating federation verdicts.** Read the verdicts with `promotion_query({artifact_ref})` \
— not from child reply JSON. Execution roles (`unit_test_runner`, `sealed_evaluator`) need an \
`execution_trace_id`; the gateway derives `pass` from it. Then **seed the revision before escalating**: \
call `agent_revision_create({agent_id, artifact_ref})` and pass the returned `revision_id` to \
`federation_escalate`. Seeding routes the escalation through the robust path (capabilities read from the \
revision record) instead of parsing the artifact's `SKILL.md` frontmatter at escalate time, which fails \
opaquely and only after the operator has been bothered. Never invent placeholder ids like `rev-initial`.\n\n\
```json\n\
federation_escalate({\n\
  \"artifact_ref\": \"<ar.* ref>\", \"agent_id\": \"<agent_id>\",\n\
  \"revision_id\": \"<rev_sha256:... from agent_revision_create>\",\n\
  \"root_session_id\": \"<root_session_id>\",\n\
  \"role_verdicts\": [\n\
    {\"role\": \"auditor\", \"agent_id\": \"auditor.default\", \"passed\": true, \"findings_summary\": \"...\", \"recorded_at\": \"...\"}\n\
  ],\n\
  \"planner_synthesis\": \"All federation roles passed. Recommend promotion.\"\n\
})\n\
```\n\n\
Returns `{approval_request_id, status: \"pending\"}` and **gates `agent_spawn` for the whole session until \
resolved** — surface the id and the resolution command, then end your turn. Do not open a second channel \
with `user_ask`; it is a separate artifact and will not resolve the gate."
            .to_string(),
    }
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
            // Signature only. The procedure — when to call it, the seeded-vs-unseeded
            // choice, the worked payload — is a phase-gated guidance block below, so
            // sessions that never reach an artifact don't pay for it (RFC
            // `docs/internals/prompt/burden-study.md`).
            description: "Escalate collected federation jury verdicts to the operator \
                 for review; returns the escalation_id. Call after the federation roles \
                 have run and their verdicts were read with promotion_query."
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
                        "description": "The seeded revision being promoted, as returned by \
                            agent_revision_create. Accepts 'rev_sha256:<hex>', 'rev_<short>', \
                            or the bare '<short>'."
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

    fn guidance(&self) -> Vec<crate::runtime::guidance::GuidanceBlock> {
        vec![escalate_procedure_block()]
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
                "Gateway store not available for federation_escalate",
                None::<String>,
            )
            .to_error_response());
        };

        // Resolve artifact_ref if provided, falling back to artifact_id if not.
        // Callers also put `ar.*` refs in artifact_id — resolve those the same way.
        //
        // Fail CLOSED on unresolved refs: a caller who passed an `ar.*` ref
        // (or `artifact_ref`) clearly meant a real artifact, and silently
        // falling back to the bare ref string as the artifact_id would bind
        // the escalation under a non-canonical key like `unseeded:ar.deadbeef`
        // and break every promote-side artifact lookup. Require a non-empty
        // session when a ref is provided, since `resolve_artifact_ref_any_scope`
        // can only resolve global (not session-scoped) refs without one.
        let sid = _session_id.unwrap_or("");
        let caller_artifact_id = if let Some(ref ref_id) = args.artifact_ref {
            if sid.is_empty() {
                return Ok(autonoetic_types::tool_error::ToolError::validation(
                    format!(
                        "artifact_ref '{}' was passed but no session_id is available to \
                         resolve it. Pass the canonical artifact_id directly, or invoke \
                         federation_escalate from within a session.",
                        ref_id
                    ),
                    None::<String>,
                )
                .to_error_response());
            }
            match store.resolve_artifact_ref_any_scope(ref_id, sid)? {
                Some(r) => r.artifact_id,
                None => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!(
                            "artifact_ref '{}' could not be resolved in this session. Pass the \
                             canonical artifact_id returned by artifact_build, or omit \
                             artifact_ref to use artifact_id.",
                            ref_id
                        ),
                        None::<String>,
                    )
                    .to_error_response());
                }
            }
        } else if args.artifact_id.starts_with("ar.") {
            // Same rule for callers that put the `ar.*` ref straight into
            // artifact_id (a common shape — agent_revision_create accepts it
            // there too). Don't silently fall back to the literal string.
            if sid.is_empty() {
                return Ok(autonoetic_types::tool_error::ToolError::validation(
                    format!(
                        "artifact_id '{}' looks like an artifact ref but no session_id is \
                         available to resolve it. Pass the canonical artifact_id instead.",
                        args.artifact_id
                    ),
                    None::<String>,
                )
                .to_error_response());
            }
            match store.resolve_artifact_ref_any_scope(&args.artifact_id, sid)? {
                Some(r) => r.artifact_id,
                None => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!(
                            "artifact_id '{}' looks like an artifact ref but could not be \
                             resolved in this session. Pass the canonical artifact_id returned \
                             by artifact_build.",
                            args.artifact_id
                        ),
                        None::<String>,
                    )
                    .to_error_response());
                }
            }
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
        //
        // Fail CLOSED on the orphaned-index edge case: if `lookup_short_id`
        // returns a full id that `get_agent_revision` then can't find, the
        // index is stale (revision was deleted, or the row was never written).
        // Treating that as "unseeded new agent" would silently re-bind the
        // approval under `unseeded:<artifact>` for what the caller meant as an
        // existing revision — refuse it explicitly.
        let resolved_revision = match args.revision_id.as_deref() {
            Some(rid) => match store.get_agent_revision(rid)? {
                Some(rev) => Some(rev),
                None => {
                    let short_lookup = rid
                        .strip_prefix("rev_")
                        .filter(|s| !s.is_empty())
                        .unwrap_or(rid);
                    match store.lookup_short_id(short_lookup)? {
                        Some(full_id) => match store.get_agent_revision(&full_id)? {
                            Some(rev) => Some(rev),
                            None => {
                                return Ok(autonoetic_types::tool_error::ToolError::validation(
                                    format!(
                                        "short revision id '{}' resolved to '{}' but no such \
                                         revision record exists (the short_id_index is stale: \
                                         the revision was deleted or its row was never written). \
                                         Re-create the revision (agent_revision_create) and \
                                         re-escalate with the returned id.",
                                        rid, full_id
                                    ),
                                    None::<String>,
                                )
                                .to_error_response());
                            }
                        },
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
                            "federation_escalate: correcting artifact id to canonical value from revision record"
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

        // ------------------------------------------------------------------
        // Federation carry-forward (Stage 3): verify any `carried_from` claims
        // in the role verdicts and materialize them onto the current artifact's
        // promotion record. The gateway verifies every claim — the agent only
        // proposes. A rejected claim fails the whole escalate call with a
        // structured `carry_forward_rejected` error naming the offending role,
        // so the planner re-runs just that gate. Runs under
        // `federation.carry_forward_strictness` (default `off`).
        // ------------------------------------------------------------------
        let strictness = _config
            .map(|c| c.federation.carry_forward_strictness)
            .unwrap_or_default();
        if args
            .role_verdicts
            .iter()
            .any(|v| v.carried_from.is_some())
        {
            let Some(gw_dir) = gateway_dir else {
                return Ok(autonoetic_types::tool_error::ToolError::resource(
                    "carry-forward verification requires a gateway directory",
                    None::<String>,
                )
                .to_error_response());
            };
            let promo_store =
                crate::runtime::promotion_store::PromotionStore::new(gw_dir)?;
            let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
            let current_bundle = artifact_store.inspect(&canonical_artifact_id)?;
            let current_digests =
                crate::runtime::federation_carry_forward::compute_federation_digests(
                    &current_bundle,
                    &artifact_store,
                );

            for verdict in &args.role_verdicts {
                let Some(cf) = &verdict.carried_from else {
                    continue;
                };
                // Resolve the prior artifact ref to its internal id, then to
                // the promotion record that (maybe) holds the prior verdict.
                let prior_resolved = super::artifact::resolve_artifact_ref_or_canonical(
                    &cf.prior_artifact_ref,
                    _session_id.unwrap_or_default(),
                    &store,
                    gw_dir,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "carry_forward_rejected: role={} reason=prior_artifact_unresolvable \
                         strictness={}: {}",
                        verdict.role.as_str(),
                        strictness.as_str(),
                        e
                    )
                })?;
                let prior_record = promo_store.get_promotion(&prior_resolved.artifact_id);

                crate::runtime::federation_carry_forward::verify_carry_claim(
                    &verdict.role,
                    prior_record.as_ref(),
                    &current_digests,
                    strictness,
                )
                .map_err(|rejection| {
                    anyhow::anyhow!(
                        "carry_forward_rejected: role={} reason={} strictness={} — {}. \
                         Re-run that gate on the current artifact and re-escalate.",
                        verdict.role.as_str(),
                        rejection.reason_code(),
                        strictness.as_str(),
                        rejection.message(&verdict.role),
                    )
                })?;

                // Accepted: materialize the carried verdict onto the current
                // artifact's record with provenance. Fail the escalate if the
                // write fails (a silently-dropped carried verdict would let the
                // FullJury gate see a missing record later).
                let prior = prior_record.as_ref().expect("verified above");
                let provenance = autonoetic_types::promotion::RoleCarryProvenance {
                    prior_artifact_ref: cf.prior_artifact_ref.clone(),
                    prior_artifact_id: prior_resolved.artifact_id.clone(),
                    original_agent_id: verdict.agent_id.clone(),
                    verified_at: chrono::Utc::now().to_rfc3339(),
                    prior_code_digest: prior.code_digest.clone(),
                    prior_contract_digest: prior.contract_digest.clone(),
                    justification: cf.justification.clone(),
                    strictness: Some(strictness.as_str().to_string()),
                };
                promo_store
                    .record_carried_verdict(
                        &canonical_artifact_id,
                        verdict.role.clone(),
                        prior,
                        provenance,
                        (
                            current_digests.code_digest.clone(),
                            current_digests.contract_digest.clone(),
                            current_digests.prose_digest.clone(),
                        ),
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "carry_forward_rejected: role={} reason=record_write_failed \
                             strictness={}: {}",
                            verdict.role.as_str(),
                            strictness.as_str(),
                            e
                        )
                    })?;

                tracing::info!(
                    target: "federation",
                    artifact_id = %canonical_artifact_id,
                    prior_artifact_ref = %cf.prior_artifact_ref,
                    role = verdict.role.as_str(),
                    strictness = strictness.as_str(),
                    "carry_forward accepted: verdict carried from prior artifact",
                );

                // Lineage: record the carry edge in the gateway store's
                // `carry_forward_lineage` ancestry table (#1067 follow-up), so
                // "which prior artifact did this artifact carry from" is
                // answerable from the store, not only from the planner's
                // naming within the workflow + a cross-session digest match.
                // Best-effort like the causal event below: the promotion
                // record's carried_roles + digests are the enforcement
                // surface; this table is the audit/answerability layer.
                if let Err(e) = store.record_carry_lineage(
                    &canonical_artifact_id,
                    verdict.role.as_str(),
                    &prior_resolved.artifact_id,
                    &cf.prior_artifact_ref,
                    strictness.as_str(),
                    prior.code_digest.as_deref(),
                    prior.contract_digest.as_deref(),
                ) {
                    tracing::warn!(
                        target: "federation",
                        artifact_id = %canonical_artifact_id,
                        role = verdict.role.as_str(),
                        error = %e,
                        "Failed to record carry-forward ancestry row",
                    );
                }

                // Audit: emit a `federation.carry_forward` causal event per
                // accepted carry (same shape as `grant_revocation`). Best-effort
                // — a failed insert is logged but does not fail the escalate
                // (the carried verdict is already durably recorded on the
                // promotion record, which is the enforcement surface).
                if let Err(e) = store.create_causal_event(
                    &autonoetic_types::causal_chain::CausalEventRecord {
                        event_id: format!("carry-fwd-{}", uuid::Uuid::new_v4()),
                        agent_id: "gateway".to_string(),
                        session_id: args.root_session_id.clone(),
                        turn_id: None,
                        event_seq: 0,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        category: "federation.carry_forward".to_string(),
                        action: "accepted".to_string(),
                        status: "completed".to_string(),
                        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
                        target: Some(canonical_artifact_id.clone()),
                        payload: Some(
                            serde_json::json!({
                                "role": verdict.role.as_str(),
                                "prior_artifact_ref": cf.prior_artifact_ref,
                                "new_artifact_id": canonical_artifact_id,
                                "strictness": strictness.as_str(),
                                "justification": cf.justification,
                            })
                            .to_string(),
                        ),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: cf.justification.clone(),
                    },
                ) {
                    tracing::warn!(
                        target: "federation",
                        artifact_id = %canonical_artifact_id,
                        error = %e,
                        "failed to emit federation.carry_forward causal event (non-blocking)"
                    );
                }
            }
        }

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
                    "federation_escalate: revision not seeded yet (new agent) — \
                     reading declared capabilities from the artifact SKILL.md"
                );
                super::agent_revision::load_artifact_capabilities(
                    gw_dir,
                    &canonical_artifact_id,
                )
                .map_err(|e| {
                    anyhow::anyhow!(
                        "revision '{}' is not seeded and the artifact's SKILL.md is unreadable: {}",
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
                    //
                    // Speak in the caller's terms: agents know the ar.* ref they
                    // passed, never the internal art_* id or the derived
                    // 'unseeded:' revision handle. Lead with the concrete fix so
                    // the agent repairs the manifest and retries this same tool
                    // call instead of pivoting to unrelated work (session-ea3df271).
                    let caller_ref = args
                        .artifact_ref
                        .as_deref()
                        .filter(|r| !r.is_empty())
                        .unwrap_or(args.artifact_id.as_str());
                    let fix_hint = if revision_seeded {
                        "Fix: the seeded revision's SKILL.md is unreadable — re-create the \
                         revision with agent_revision_create using a SKILL.md whose YAML \
                         frontmatter opens with a '---' line and closes with a matching '---' \
                         line, then re-escalate with the new revision_id."
                    } else {
                        "The escalation reads capabilities from the artifact's SKILL.md YAML \
                         frontmatter, which must open with a '---' line and close with a \
                         matching '---' line. Fix: content_write a corrected SKILL.md with \
                         valid frontmatter, rebuild with artifact_build, then call \
                         federation_escalate again with the new artifact_ref. Do not continue \
                         to other work — repair the manifest and retry this same call."
                    };
                    return Ok(autonoetic_types::tool_error::ToolError::execution(
                        format!(
                            "could not load declared capabilities for '{}' from artifact \
                             '{}': {}. Refusing to downgrade the promotion review to \
                             jury-only (R++2 fail-closed). {}",
                            args.agent_id, caller_ref, e, fix_hint
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

            // #1094: promotion-identity dedup. The merged card exists so ONE
            // operator decision covers both the R++2 capability delta and the
            // jury verdicts. When a bare `RevisionPromote` approval for the
            // same promotion identity `(agent, revision, outgoing, added,
            // broadened)` already exists, the operator has already been asked
            // about this promotion — do NOT mint a second card (observed
            // double-approval: session-53043b4c, apr-6150b08d + apr-23d36590):
            //   approved (all verdicts pass) → escalation auto-cleared and
            //       linked to that approval; the promote side (FullJury + R++2)
            //       consumes the single decision via the approved projection.
            //   pending                        → escalation linked to it; the
            //       one pending decision resolves both the approval and the
            //       projection when the operator decides.
            //   rejected                       → refuse the escalate without
            //       re-asking the operator for a decision they already made.
            // Approved-with-failed-verdicts falls through to the merged card
            // (the verdicts still need a fresh operator review). The mirror
            // direction (escalation first, promote second) is #738: the
            // promote reuses the merged approval via its federation context.
            // The lookup is scoped to this escalation's root session — an
            // approval minted under a different root is a different operator
            // context and must not suppress this decision.
            let verdicts_all_pass = escalation.role_verdicts.iter().all(|v| v.passed);
            if let Some(existing) = store
                .find_matching_revision_promote_approval_for_identity(
                    &args.root_session_id,
                    &args.agent_id,
                    &canonical_revision_id,
                    &outgoing_revision_id,
                    &added_capabilities,
                    &broadened_capabilities,
                )?
            {
                // Note: the store helper surfaces pending as `status: None`
                // ("pending" is the query's catch-all), approved/rejected
                // explicitly. None here therefore means pending.
                match existing.status {
                    Some(autonoetic_types::background::ApprovalStatus::Approved)
                        if verdicts_all_pass =>
                    {
                        escalation.status = EscalationStatus::Approved;
                        escalation.approval_request_id = Some(existing.request_id.clone());
                        escalation.decided_by = Some(
                            existing
                                .decided_by
                                .clone()
                                .unwrap_or_else(|| "gate_service".to_string()),
                        );
                        escalation.resolved_at = Some(chrono::Utc::now().to_rfc3339());
                        store.create_escalation(&mut escalation)?;
                        return Ok(serde_json::json!({
                            "ok": true,
                            "escalation_id": escalation_id,
                            "approval_request_id": existing.request_id,
                            "status": "approved",
                            "message": format!(
                                "Capability delta for this promotion was already operator-approved \
                                 via '{}'; escalation marked approved without a second ask.",
                                existing.request_id
                            ),
                        })
                        .to_string());
                    }
                    None => {
                        // A pending approval for this same promotion exists —
                        // surface it; the operator's single decision covers both.
                        escalation.approval_request_id = Some(existing.request_id.clone());
                        store.create_escalation(&mut escalation)?;
                        return Ok(serde_json::json!({
                            "ok": true,
                            "escalation_id": escalation_id,
                            "approval_request_id": existing.request_id,
                            "status": "pending",
                            "message": format!(
                                "An approval request for this promotion is already pending \
                                 ('{}'); the operator's single decision will cover this \
                                 escalation. No second card was created.",
                                existing.request_id
                            ),
                        })
                        .to_string());
                    }
                    Some(autonoetic_types::background::ApprovalStatus::Rejected) => {
                        return Ok(autonoetic_types::tool_error::ToolError::permission(
                            format!(
                                "Promotion of '{}' revision '{}' was already REJECTED by the \
                                 operator (approval '{}'). Refusing to re-ask the same promotion \
                                 decision (R++2). Escalate again only for a new revision or a new \
                                 decision.",
                                args.agent_id, canonical_revision_id, existing.request_id
                            ),
                        )
                        .to_error_response());
                    }
                    // Approved-but-verdicts-failed, or any status the query
                    // cannot surface: fall through and mint the merged card.
                    _ => {}
                }
            }
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
            let outcome = if v.passed { "pass" } else { "fail" };
            // Mark carried verdicts distinctly so the operator cannot mistake
            // them for freshly-run gates (federation carry-forward, Stage 4).
            if let Some(cf) = &v.carried_from {
                format!("{}: {} (carried from {})", role, outcome, cf.prior_artifact_ref)
            } else {
                format!("{}: {}", role, outcome)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::escalation::CarriedFrom;
    use autonoetic_types::promotion::PromotionRole;

    fn verdict(role: PromotionRole, carried_from: Option<CarriedFrom>) -> RoleVerdictSummary {
        RoleVerdictSummary {
            role,
            agent_id: "gate.default".to_string(),
            passed: true,
            findings_summary: "ok".to_string(),
            evidence_ref: None,
            recorded_at: "2026-01-01T00:00:00Z".to_string(),
            carried_from,
        }
    }

    #[test]
    fn summary_marks_carried_verdicts_distinctly() {
        let carried = verdict(
            PromotionRole::Auditor,
            Some(CarriedFrom {
                prior_artifact_ref: "ar.prior123".to_string(),
                role: PromotionRole::Auditor,
                justification: Some("prose-only fix".to_string()),
            }),
        );
        let fresh = verdict(PromotionRole::UnitTestRunner, None);
        let summary = summarize_role_verdicts(&[carried, fresh]);
        assert!(
            summary.contains("auditor: pass (carried from ar.prior123)"),
            "carried verdict must be marked: {summary}"
        );
        assert!(
            summary.contains("unit_test_runner: pass"),
            "fresh verdict must be unmarked: {summary}"
        );
        assert!(
            !summary.contains("unit_test_runner: pass (carried"),
            "fresh verdict must not carry the marker"
        );
    }

    #[test]
    fn summary_without_carried_is_plain() {
        let a = verdict(PromotionRole::Auditor, None);
        let b = verdict(PromotionRole::SealedEvaluator, None);
        let summary = summarize_role_verdicts(&[a, b]);
        assert!(!summary.contains("carried from"));
    }
}
