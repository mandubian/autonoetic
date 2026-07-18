use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{
    plan_envelope_diff, validate_step_dag, PlanFrame, PlanFrameSummary, PlanRef, PlanStatus,
    PlanStep, StepOwner, StepStatus, ValidationEntry, ValidationPolicy,
};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(PlanFrameProposeTool));
    registry.register(Box::new(PlanFrameGetTool));
    registry.register(Box::new(PlanFrameListTool));
    registry.register(Box::new(PlanFrameApproveTool));
    registry.register(Box::new(PlanFrameAmendTool));
    registry.register(Box::new(PlanFrameHistoryTool));
}

/// Pillar C: materialize the approved plan's declared network envelope into
/// a single `RootSession`-scoped session approval grant. Subsequent tool calls
/// (`sandbox_exec`, `web_fetch`, `artifact_exec`, `artifact_prepare`,
/// `credential` URL gating) that target these hosts then dedup silently against
/// the existing `session_approval_grants` coverage check — the operator
/// approves the envelope once, not each tool call.
///
/// The envelope is derived MECHANICALLY from `plan.capability_envelope` when
/// non-empty, otherwise from each plan step's `agent_id` → declared
/// `Capability::NetworkAccess.hosts` (never LLM-judged). Wildcards
/// (`"*"`) are skipped because they don't materialize to a concrete,
/// matchable grant and would defeat the dedup's concreteness rule (the exec
/// cache only auto-approves when all patterns are `url_literal`/`ip_address`).
///
/// Best-effort: any failure (missing config, agent not installed, DB error)
/// returns 0 and the approval still succeeds. The grant carries
/// `source_approval_id = Some(plan_id)` so a later envelope-expanding amend
/// can revoke it surgically via `revoke_session_grants_by_source`.
///
/// Returns 1 if a plan grant row was materialized, 0 otherwise. The
/// whole envelope is inserted as a single `RootSession`-scoped grant
/// row (multiple `ExactHost` targets inside), so the return is binary.
/// (Symmetric with `revoke_session_grants_by_source` which returns a
/// grant-row count.) The host count itself is internal; the response
/// surfaces `grants_materialized` as this 0/1 value.
fn materialize_plan_grants(
    store: &crate::scheduler::gateway_store::GatewayStore,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    plan: &PlanFrame,
    approver: &str,
    _now: &str,
) -> usize {
    let Some(config) = config else { return 0 };

    let mut declared_hosts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for cap in &plan.capability_envelope {
        if let Capability::NetworkAccess { hosts } = cap {
            for h in hosts {
                if h == "*" || h.is_empty() {
                    continue;
                }
                declared_hosts.insert(h.clone());
            }
        }
    }
    if !declared_hosts.is_empty() {
        let hosts_vec: Vec<String> = declared_hosts.into_iter().collect();
        return crate::runtime::session_envelope::materialize_network_grant(
            store,
            &plan.root_session_id,
            &hosts_vec,
            approver,
            &plan.plan_id,
            Some(&plan.created_by_agent_id),
        );
    }

    let repo = crate::AgentRepository::from_config(config);
    let mut hosts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for step in &plan.steps {
        let raw = step.agent_id.as_deref().unwrap_or("").trim();
        if raw.is_empty() {
            continue;
        }
        let loaded = match repo.get_sync(raw) {
            Ok(l) => l,
            Err(_) => continue,
        };
        for cap in &loaded.manifest.capabilities {
            if let autonoetic_types::capability::Capability::NetworkAccess { hosts: decl } = cap {
                for h in decl {
                    if h == "*" {
                        continue;
                    }
                    hosts.insert(h.clone());
                }
            }
        }
    }
    if hosts.is_empty() {
        return 0;
    }
    let hosts_vec: Vec<String> = hosts.into_iter().collect();
    crate::runtime::session_envelope::materialize_network_grant(
        store,
        &plan.root_session_id,
        &hosts_vec,
        approver,
        &plan.plan_id,
        Some(&plan.created_by_agent_id),
    )
}

fn has_plan_frame_access(manifest: &AgentManifest) -> bool {
    manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::PlanFrameAccess { .. }))
}

/// RFC #777 Part C — run the capability preflight over `plan` and return
/// the response-facing view.
///
/// Returns `None` only when no step opted in (`required_capabilities`
/// empty everywhere) — the common case, and the only condition under
/// which the propose/amend response omits the `capability_preflight`
/// field entirely. In every other case the preflight runs and the view
/// is returned, even if every step is `Covered` (clean-but-asked is
/// surfaced with `warnings: []` so the caller can tell opt-in from
/// opt-out).
///
/// Repository/scan failures are not special-cased: a missing agent or
/// unreadable SKILL.md simply surfaces as an `agent_not_installed`
/// finding for that step, which is exactly the contract the planner
/// branches on. The preflight itself is purely static
/// (`required_capabilities` vs. declared capabilities) — no LLM, no
/// network, no judgment.
fn compute_preflight_view(
    config: &GatewayConfig,
    plan: &PlanFrame,
) -> Option<crate::runtime::plan_preflight::PreflightView> {
    use crate::runtime::plan_preflight::{preflight_plan, PreflightView};

    // Skip the directory scan entirely when no step opted into preflight
    // (the common case). Avoids touching the filesystem on every propose.
    let any_caps = plan
        .steps
        .iter()
        .any(|s| !s.required_capabilities.is_empty());
    if !any_caps {
        return None;
    }

    let repo = crate::AgentRepository::from_config(config);
    let result = preflight_plan(plan, &repo);
    let view = PreflightView::from_result(&result);
    if view.is_empty() {
        None
    } else {
        Some(view)
    }
}

/// Whether a set of `PlanFrameAccess` patterns grants `operation`. Pure so it
/// is unit-testable without constructing a full manifest.
///
/// Empty/whitespace patterns — and degenerate prefixes like `"."` that trim to
/// empty — grant nothing: otherwise `operation.starts_with("")` would silently
/// authorize every participation op (an authorization footgun, since the
/// capability schema does not forbid empty strings).
fn patterns_allow(patterns: &[String], operation: &str) -> bool {
    autonoetic_types::capability::AuthorityOp::patterns_allow(patterns, operation)
}

fn can_perform(manifest: &AgentManifest, operation: &str) -> bool {
    manifest.capabilities.iter().any(|c| match c {
        Capability::PlanFrameAccess { patterns } => patterns_allow(patterns, operation),
        _ => false,
    })
}

fn new_plan_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("plan-{}", hex::encode(&bytes.as_bytes()[..6]))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn parse_capability_envelope_input(
    items: Option<Vec<serde_json::Value>>,
) -> anyhow::Result<Vec<Capability>> {
    let Some(items) = items else {
        return Ok(Vec::new());
    };
    items
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            crate::runtime::tools::agent_revision::normalize_capability_from_llm(v)
                .map_err(|e| anyhow::anyhow!("capability_envelope[{i}]: {e}"))
        })
        .collect()
}

/// Deterministic approval request ID for a specific plan revision.
/// This lets the plan-frame tools and `apply_decision` address the same row
/// without adding a foreign-key column to `plan_frames`.
pub fn plan_approval_request_id(plan_id: &str, version: u32) -> String {
    format!("apr-plan-{plan_id}-v{version}")
}

/// Close a still-pending plan revision superseded by a newer amendment: cancel
/// its approval gate, mark the revision `cancelled`, and emit `plan.withdrawn`
/// so timeline consumers stop offering the old gate.
fn supersede_pending_plan_revision(
    store: &crate::scheduler::gateway_store::GatewayStore,
    plan_id: &str,
    old_version: u32,
    superseded_by: u32,
    session_id: &str,
    root_session_id: &str,
    actor_id: &str,
) {
    use autonoetic_types::session_timeline::TimelineRefs;

    let now = now_rfc3339();
    let old_request_id = plan_approval_request_id(plan_id, old_version);
    // Only supersede if we actually cancelled a still-pending gate.
    // `cancel_approval` errors when the row is no longer pending (rows == 0):
    // if the operator decided (approved/rejected) the old revision concurrently,
    // we must NOT withdraw an already-decided revision. Bail without touching
    // the revision status or emitting `plan.withdrawn`.
    if let Err(e) = store.cancel_approval(&old_request_id, actor_id, &now) {
        tracing::debug!(
            target: "plan_frame",
            error = %e,
            plan_id = %plan_id,
            version = %old_version,
            "skip supersede: prior revision approval no longer pending (decided concurrently or absent)"
        );
        return;
    }
    if let Err(e) = store.update_plan_frame_status(
        plan_id,
        old_version,
        PlanStatus::Cancelled,
        Some(actor_id),
        Some(&now),
    ) {
        tracing::warn!(
            target: "plan_frame",
            error = %e,
            plan_id = %plan_id,
            version = %old_version,
            "failed to mark superseded plan revision cancelled"
        );
    }

    let role = crate::runtime::session_timeline::derive_role(actor_id);
    let principal = autonoetic_types::principal::Principal::agent(actor_id.to_string());
    let refs = TimelineRefs {
        plan_id: Some(plan_id.to_string()),
        approval_request_id: Some(old_request_id),
        ..Default::default()
    };
    let event = crate::runtime::session_timeline::build_timeline_event(
        root_session_id.to_string(),
        session_id.to_string(),
        None,
        &principal,
        &role,
        "plan.withdrawn",
        None,
        Some(serde_json::json!({
            "plan_id": plan_id,
            "version": old_version,
            "superseded_by": superseded_by,
            "reason": "superseded by amended revision",
        })),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            plan_id = %plan_id,
            version = %old_version,
            "plan.withdrawn timeline emit failed (supersede)"
        );
    }
}

/// Outcome of gating a plan revision through `GateService`.
#[derive(Debug)]
enum PlanGateOutcome {
    /// Approval row exists or was created; the plan stays awaiting approval.
    Pending,
    /// The gate cleared immediately (existing approval, session grant, or policy);
    /// the caller should mark the plan approved.
    Approved,
}

/// Create the canonical `ApprovalRequest` for a plan revision via `GateService`.
/// The approval row lives in the standard `approvals` table; the plan content
/// remains in `plan_frames`.
fn create_plan_approval_request(
    store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    plan: &PlanFrame,
    manifest: &AgentManifest,
    session_id: &str,
    config: &GatewayConfig,
    turn_id: Option<&str>,
) -> anyhow::Result<PlanGateOutcome> {
    use crate::runtime::human_gate::{
        DecisionContext, GateKind, GateRequest, GateResult, GateService, MatchStrategy,
    };
    use autonoetic_types::background::ScheduledAction;

    let request_id = plan_approval_request_id(&plan.plan_id, plan.version);
    let action = ScheduledAction::PlanFrame {
        plan_id: plan.plan_id.clone(),
        version: plan.version,
        envelope: plan.capability_envelope.clone(),
    };

    let gate_service = GateService::new(store);
    let gate_req = GateRequest {
        kind: GateKind::Approval {
            action: action.clone(),
            // `targets` is intentionally empty: PlanFrame carries no host
            // targets. Dedup safety relies on MatchStrategy::ExactPayload, which
            // short-circuits in find_pending_for_targets via exact_payload_covers
            // (full structural equality of the action, including plan_id +
            // version) *before* the "empty targets → any pending of same kind"
            // fallback runs. Do NOT switch this to a looser strategy without also
            // giving PlanFrame real targets, or unrelated pending plan approvals
            // (different plan_id/version) would collapse onto each other and the
            // explicit request_id below would be bypassed (#724 Part B review).
            targets: Vec::new(),
            match_strategy: MatchStrategy::ExactPayload,
        },
        manifest,
        session_id: Some(session_id),
        run_context: None,
        config: Some(config),
        context: DecisionContext::tier2(
            format!("Plan '{}' version {}", plan.plan_id, plan.version),
            "Plan frame proposed or amended",
            format!(
                "Capability envelope with {} item(s). Approving materializes grants.",
                plan.capability_envelope.len()
            ),
            "Approve if the plan steps and capability envelope are acceptable; reject if not",
        ),
        summary: format!("Plan {} v{} approval", plan.plan_id, plan.version),
        approval_ref: None,
        request_id: Some(&request_id),
        pre_validated: false,
        cache_backfill: None,
        turn_id,
    };

    match gate_service.check(gate_req)? {
        GateResult::AlreadyPending { .. } | GateResult::Suspended { .. } => {
            Ok(PlanGateOutcome::Pending)
        }
        GateResult::Cleared { source, .. } => {
            tracing::info!(
                target: "plan_frame",
                plan_id = %plan.plan_id,
                version = plan.version,
                source = ?source,
                "Plan approval gate cleared via GateService"
            );
            Ok(PlanGateOutcome::Approved)
        }
        GateResult::PolicyAllowed => Ok(PlanGateOutcome::Approved),
    }
}

pub struct PlanFrameProposeTool;

impl NativeTool for PlanFrameProposeTool {
    fn name(&self) -> &'static str {
        "planframe_propose"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Propose a new PlanFrame for collaborative work. Creates a workflow if one does not exist yet. The plan starts in 'awaiting_approval' status and must be approved before agents act on it. The response carries a `capability_preflight` field (RFC #777 Part C) when any step declares `required_capabilities`: a deterministic, advisory check that the intended `agent_id` declares those capability types. Warnings do not block — branch on them (re-delegate, decompose, escalate) or proceed on the record.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short title for the plan"
                    },
                    "objective": {
                        "type": "string",
                        "description": "Detailed objective and acceptance criteria"
                    },
                    "steps": {
                        "type": "array",
                        "description": "Ordered list of plan steps",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "title": { "type": "string" },
                                "owner": { "type": "string", "enum": ["planner", "agent", "operator", "shared"] },
                                "agent_id": { "type": "string" },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "notes": { "type": "string" },
                                "required_capabilities": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Capability type names this step requires (e.g. [\"NetworkAccess\", \"CodeExecution\"]). When non-empty AND agent_id is set, the gateway runs an advisory capability preflight at plan time and surfaces findings in the response's `capability_preflight`. Does not block — proceeding past a warning is on the record. Valid names match Capability::type_name: SandboxFunctions, ReadAccess, WriteAccess, NetworkAccess, AgentSpawn, AgentMessage, BackgroundReevaluation, CodeExecution, ArtifactExecution, EmergencyStop, AgentRevision, Evaluation, ApprovalQueue, SchedulerSignal, CredentialAccess, UserProfileAccess, SchedulerAccess, SkillInstall, ConstitutionalProposal, ReasoningAudit, GithubIssueCreate, budget.no_price_available.allow, SecurityRedTeam, CapsuleExport, SelfCapsuleExport, PlanFrameAccess, WikiContribute, PromoteWith, GateDecider."
                                }
                            },
                            "required": ["step_id"]
                        }
                    },
                    "validation_policy": {
                        "type": "object",
                        "description": "Validation checks required/advisory for this plan",
                        "properties": {
                            "entries": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "validation_id": { "type": "string" },
                                        "title": { "type": "string" },
                                        "class": { "type": "string", "enum": ["mechanical_safety", "security_review", "correctness_check", "quality_check", "packaging_check"] },
                                        "requirement": { "type": "string", "enum": ["required", "advisory"] }
                                    },
                                    "required": ["validation_id", "title"]
                                }
                            }
                        }
                    },
                    "capability_envelope": {
                        "type": "array",
                        "description": "Optional session capability envelope to propose at plan approval (e.g. NetworkAccess hosts discovered during research)",
                        "items": { "type": "object" }
                    }
                },
                "required": ["title", "objective"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest) && can_perform(manifest, "planframe.propose")
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct StepInput {
            step_id: String,
            title: String,
            owner: Option<String>,
            agent_id: Option<String>,
            depends_on: Option<Vec<String>>,
            notes: Option<String>,
            required_capabilities: Option<Vec<String>>,
        }

        #[derive(Deserialize)]
        struct ValidationInput {
            validation_id: String,
            title: String,
            class: Option<String>,
            requirement: Option<String>,
        }

        #[derive(Deserialize)]
        struct ValidationPolicyInput {
            entries: Option<Vec<ValidationInput>>,
        }

        #[derive(Deserialize)]
        struct Args {
            title: String,
            objective: String,
            steps: Option<Vec<StepInput>>,
            validation_policy: Option<ValidationPolicyInput>,
            capability_envelope: Option<Vec<serde_json::Value>>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let Some(config) = config else {
            return Ok(ToolError::execution(
                "Gateway config not available",
                Some("Ensure the gateway configuration is loaded and valid."),
            )
            .with_code("gateway_config_unavailable")
            .to_error_response());
        };

        let session_id = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root_session_id = session_id.split('/').next().unwrap_or(session_id);

        let workflow = crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            config,
            Some(&store),
            root_session_id,
            Some(&manifest.agent.id),
        )?;

        let plan_id = new_plan_id();
        let now = now_rfc3339();

        let steps: Vec<PlanStep> = args
            .steps
            .unwrap_or_default()
            .into_iter()
            .map(|s| PlanStep {
                step_id: s.step_id,
                title: s.title,
                owner: match s.owner.as_deref() {
                    Some("agent") => StepOwner::Agent,
                    Some("operator") => StepOwner::Operator,
                    Some("shared") => StepOwner::Shared,
                    _ => StepOwner::Planner,
                },
                depends_on: s.depends_on.unwrap_or_default(),
                agent_id: s.agent_id,
                notes: s.notes,
                status: StepStatus::Pending,
                required_capabilities: s.required_capabilities.unwrap_or_default(),
            })
            .collect();

        let validation_policy = match args.validation_policy {
            Some(vp) => ValidationPolicy {
                entries: vp
                    .entries
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| ValidationEntry {
                        validation_id: v.validation_id,
                        title: v.title,
                        class: match v.class.as_deref() {
                            Some("security_review") => {
                                autonoetic_types::plan_frame::ValidationClass::SecurityReview
                            }
                            Some("correctness_check") => {
                                autonoetic_types::plan_frame::ValidationClass::CorrectnessCheck
                            }
                            Some("quality_check") => {
                                autonoetic_types::plan_frame::ValidationClass::QualityCheck
                            }
                            Some("packaging_check") => {
                                autonoetic_types::plan_frame::ValidationClass::PackagingCheck
                            }
                            _ => autonoetic_types::plan_frame::ValidationClass::MechanicalSafety,
                        },
                        requirement: match v.requirement.as_deref() {
                            Some("advisory") => {
                                autonoetic_types::plan_frame::ValidationRequirement::Advisory
                            }
                            Some("waived") => {
                                autonoetic_types::plan_frame::ValidationRequirement::Waived
                            }
                            _ => autonoetic_types::plan_frame::ValidationRequirement::Required,
                        },
                    })
                    .collect(),
            },
            None => ValidationPolicy::default(),
        };

        let capability_envelope = parse_capability_envelope_input(args.capability_envelope)?;

        // Validate the step dependency graph before persisting.
        if let Err(dag_err) = validate_step_dag(&steps) {
            return Ok(ToolError::validation(
                &dag_err,
                Some("Fix depends_on entries so they reference existing step_ids with no cycles."),
            )
            .with_code("invalid_step_dependencies")
            .to_error_response());
        }

        let mut plan = PlanFrame {
            plan_id: plan_id.clone(),
            version: 1,
            parent_version: None,
            workflow_id: workflow.workflow_id.clone(),
            root_session_id: root_session_id.to_string(),
            title: args.title,
            objective: args.objective,
            status: PlanStatus::AwaitingApproval,
            steps,
            validation_policy,
            capability_envelope,
            approved_by: None,
            approved_at: None,
            created_by_agent_id: manifest.agent.id.clone(),
            reason: None,
            created_at: now,
            expires_at: {
                let ttl = config.plan_frame_timeout_secs;
                if ttl == 0 {
                    None
                } else {
                    Some((chrono::Utc::now() + chrono::Duration::seconds(ttl as i64)).to_rfc3339())
                }
            },
        };

        store.save_plan_frame(&plan)?;

        // Unify plan approval with the standard ApprovalRequest system (#565).
        // The plan content stays in `plan_frames`; the gate lives in `approvals`.
        match create_plan_approval_request(
            store.clone(),
            &plan,
            manifest,
            session_id,
            config,
            _turn_id,
        ) {
            Ok(PlanGateOutcome::Approved) => {
                plan.status = PlanStatus::Approved;
                plan.approved_by = Some("gate_service".to_string());
                plan.approved_at = Some(now_rfc3339());
                if let Err(e) = store.save_plan_frame(&plan) {
                    tracing::warn!(
                        target: "plan_frame",
                        error = %e,
                        plan_id = %plan.plan_id,
                        "failed to mark gate-cleared plan as approved"
                    );
                }
            }
            Ok(PlanGateOutcome::Pending) => {}
            Err(e) => {
                tracing::warn!(
                    target: "plan_frame",
                    error = %e,
                    plan_id = %plan.plan_id,
                    "failed to create plan approval request"
                );
            }
        }

        // Canonical timeline: a plan proposal is an `attention` gate (#363 P1).
        {
            let role = crate::runtime::session_timeline::derive_role(&plan.created_by_agent_id);
            let principal =
                autonoetic_types::principal::Principal::agent(plan.created_by_agent_id.clone());
            let refs = autonoetic_types::session_timeline::TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            };
            let event = crate::runtime::session_timeline::build_timeline_event(
                root_session_id.to_string(),
                session_id.to_string(),
                _turn_id.map(str::to_string),
                &principal,
                &role,
                "plan.pending",
                None,
                Some(serde_json::json!({
                    "plan_id": plan_id,
                    "version": plan.version,
                    "title": plan.title,
                })),
                refs,
            );
            if let Err(e) = store.create_live_digest_event(&event) {
                tracing::debug!(target: "session_timeline", error = %e, "plan.pending timeline emit failed");
            }
        }

        let mut updated_workflow = workflow;
        updated_workflow.active_plan_ref = Some(PlanRef {
            plan_id: plan_id.clone(),
            version: 1,
        });
        updated_workflow.updated_at = now_rfc3339();
        crate::scheduler::workflow_store::save_workflow_run(
            config,
            Some(&store),
            &updated_workflow,
        )?;

        let event_id = {
            let bytes = uuid::Uuid::new_v4();
            format!("evt-{}", hex::encode(&bytes.as_bytes()[..8]))
        };
        crate::scheduler::workflow_store::append_workflow_event(
            config,
            Some(&store),
            &autonoetic_types::workflow::WorkflowEventRecord {
                event_id,
                workflow_id: updated_workflow.workflow_id.clone(),
                task_id: None,
                event_type: "planframe.proposed".to_string(),
                agent_id: Some(manifest.agent.id.clone()),
                payload: serde_json::json!({
                    "plan_id": plan_id,
                    "title": plan.title,
                    "step_count": plan.steps.len(),
                    "step_titles": plan.steps.iter().map(|s| s.title.clone()).collect::<Vec<_>>(),
                }),
                occurred_at: now_rfc3339(),
            },
        )?;

        // Optional auto-approval (config: `plan_auto_approve`). A convenience for
        // local/dev and autonomous runs with no operator in the loop. OFF by
        // default, so separation of powers holds unless explicitly enabled. The
        // approver is recorded as `plan_auto_approver` so the audit trail shows it
        // was an automatic decision, not a human/agent authority. (#602 follow-up)
        // `config` was unwrapped to `&GatewayConfig` above (early return otherwise).
        let mut effective_status = "awaiting_approval";
        let mut auto_approved = false;
        {
            if config.plan_auto_approve {
                let approver = config.plan_auto_approver.clone();
                let request_id = plan_approval_request_id(&plan.plan_id, plan.version);
                match crate::scheduler::approval::approve_request(
                    config,
                    Some(&store),
                    &request_id,
                    &approver,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(decision) => {
                        let now2 = now_rfc3339();
                        let grants = materialize_plan_grants(
                            &store,
                            Some(config),
                            &plan,
                            &decision.decided_by,
                            &now2,
                        );
                        let _ = crate::scheduler::workflow_store::append_workflow_event(
                            config,
                            Some(&store),
                            &autonoetic_types::workflow::WorkflowEventRecord {
                                event_id: {
                                    let b = uuid::Uuid::new_v4();
                                    format!("evt-{}", hex::encode(&b.as_bytes()[..8]))
                                },
                                workflow_id: plan.workflow_id.clone(),
                                task_id: None,
                                event_type: "planframe.approved".to_string(),
                                agent_id: Some(approver.clone()),
                                payload: serde_json::json!({
                                    "plan_id": plan.plan_id,
                                    "version": plan.version,
                                    "grants_materialized": grants,
                                    "auto_approved": true,
                                }),
                                occurred_at: now2,
                            },
                        );
                        effective_status = "approved";
                        auto_approved = true;
                        tracing::info!(
                            target: "plan_frame",
                            plan_id = %plan.plan_id,
                            approver = %approver,
                            "plan auto-approved (plan_auto_approve=true)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "plan_frame",
                            error = %e,
                            plan_id = %plan.plan_id,
                            "auto-approve failed; plan left awaiting_approval"
                        );
                    }
                }
            }
        }

        let summary = plan.compact_summary();

        // RFC #777 Part C — plan-time capability preflight. Advisory: returns
        // findings (uncovered capabilities / not-installed agents) but never
        // blocks. The planner branches on them; if it proceeds anyway, the
        // warnings are part of the tool response the LLM saw, so "on the
        // record" is satisfied. Empty when no step declares
        // `required_capabilities` — fully opt-in per the RFC.
        let preflight_view = compute_preflight_view(config, &plan);

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": plan_id,
            "workflow_id": updated_workflow.workflow_id,
            "status": effective_status,
            "version": 1,
            "message": if auto_approved {
                "Plan proposed and auto-approved (plan_auto_approve=true). Proceed to execution."
            } else {
                "Plan proposed. Approval by an authority is required before agents can act on it."
            },
            "auto_approved": auto_approved,
            "summary": summary,
            "capability_preflight": preflight_view,
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct PlanFrameGetTool;

impl NativeTool for PlanFrameGetTool {
    fn name(&self) -> &'static str {
        "planframe_get"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Get a PlanFrame by plan_id (latest version), or a specific revision by version. Omit plan_id to get the active plan for the current workflow.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to retrieve. Omit to get the active plan for the current workflow."
                    },
                    "version": {
                        "type": "integer",
                        "description": "Specific revision version to retrieve. Omit for latest."
                    },
                    "compact": {
                        "type": "boolean",
                        "description": "If true, return a compact summary instead of the full plan."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            plan_id: Option<String>,
            version: Option<u32>,
            compact: Option<bool>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let plan = if let Some(pid) = &args.plan_id {
            if let Some(ver) = args.version {
                store.load_plan_frame_revision(pid, ver)?
            } else {
                store.load_plan_frame(pid)?
            }
        } else {
            let sid = session_id
                .ok_or_else(|| anyhow::anyhow!("session_id required when plan_id not specified"))?;
            let root = sid.split('/').next().unwrap_or(sid);
            let wf_id = store.resolve_workflow_id(root)?;
            match wf_id {
                Some(wid) => store.load_active_plan_for_workflow(&wid)?,
                None => None,
            }
        };

        match plan {
            Some(p) => {
                if args.compact.unwrap_or(false) {
                    Ok(serde_json::to_string(&serde_json::json!({
                        "ok": true,
                        "summary": p.compact_summary(),
                    }))?)
                } else {
                    let mut body = serde_json::json!({
                        "ok": true,
                        "plan": p,
                    });
                    if let Some(hint) = p.execution_wake_hint() {
                        body["execution_hint"] = serde_json::Value::String(hint);
                    }
                    Ok(serde_json::to_string(&body)?)
                }
            }
            None => Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "plan": null,
                "message": "No plan found"
            }))?),
        }
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct PlanFrameListTool;

impl NativeTool for PlanFrameListTool {
    fn name(&self) -> &'static str {
        "planframe_list"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List all PlanFrames for the current workflow (latest revision of each). Returns compact summaries.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root = sid.split('/').next().unwrap_or(sid);
        let wf_id = store.resolve_workflow_id(root)?;

        let plans = match wf_id {
            Some(wid) => store.list_plan_frames_for_workflow(&wid)?,
            None => vec![],
        };

        let summaries: Vec<PlanFrameSummary> = plans.iter().map(|p| p.compact_summary()).collect();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plans": summaries,
            "count": summaries.len(),
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct PlanFrameApproveTool;

impl NativeTool for PlanFrameApproveTool {
    fn name(&self) -> &'static str {
        "planframe_approve"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Approve a PlanFrame (status 'awaiting_approval' → 'approved'). Approval is an AUTHORITY: it requires the explicit `planframe.approve` right (a `*` wildcard does NOT grant it), so a proposing agent cannot approve its own plan. Exercised by the operator through the gateway, or by an agent explicitly granted the right.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to approve (approves the latest revision)"
                    },
                    "approved_by": {
                        "type": "string",
                        "description": "Identity of the approver (e.g., 'operator', user ID)"
                    }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest) && can_perform(manifest, "planframe.approve")
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
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            plan_id: String,
            approved_by: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let Some(plan) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(ToolError::not_found(
                "Plan",
                Some("Create a plan first or check the plan ID."),
            )
            .with_code("plan_not_found")
            .to_error_response());
        };

        if plan.status != PlanStatus::AwaitingApproval {
            return Ok(ToolError::conflict(
                format!(
                    "Plan is in '{}' status; only awaiting_approval plans can be approved",
                    plan.status.as_str()
                ),
                Some("Ensure the plan is in AwaitingApproval status before approving."),
            )
            .with_code("plan_wrong_status")
            .to_error_response());
        }

        let Some(config) = config else {
            return Ok(ToolError::execution(
                "Gateway config not available",
                Some("Ensure the gateway configuration is loaded and valid."),
            )
            .with_code("gateway_config_unavailable")
            .to_error_response());
        };

        let approver = args
            .approved_by
            .unwrap_or_else(|| manifest.agent.id.clone());
        let request_id = plan_approval_request_id(&plan.plan_id, plan.version);

        // Route through the standard approval decision path (#565). This calls
        // `apply_decision`, which updates the plan status and materializes the
        // declared capability envelope as session approval grants.
        let decision = match crate::scheduler::approval::approve_request(
            config,
            Some(&store),
            &request_id,
            &approver,
            None,
            None,
            None,
            None,
        ) {
            Ok(d) => d,
            Err(e) => {
                return Ok(ToolError::execution(
                    format!("Plan approval failed: {e}"),
                    Some("The approval request may no longer be pending or the decider lacks authorization."),
                ).with_code("plan_approval_failed").to_error_response());
            }
        };

        let now = now_rfc3339();

        // `apply_decision` already materialized grants; re-run the pure
        // count path to surface `grants_materialized` in the tool response.
        // The grant insertion is idempotent, so this is safe.
        let grants_materialized =
            materialize_plan_grants(&store, Some(config), &plan, &decision.decided_by, &now);

        if let Err(e) = crate::scheduler::workflow_store::append_workflow_event(
            config,
            Some(&store),
            &autonoetic_types::workflow::WorkflowEventRecord {
                event_id: {
                    let bytes = uuid::Uuid::new_v4();
                    format!("evt-{}", hex::encode(&bytes.as_bytes()[..8]))
                },
                workflow_id: plan.workflow_id.clone(),
                task_id: None,
                event_type: "planframe.approved".to_string(),
                agent_id: Some(manifest.agent.id.clone()),
                payload: serde_json::json!({
                    "plan_id": plan.plan_id,
                    "version": plan.version,
                    "grants_materialized": grants_materialized,
                }),
                occurred_at: now,
            },
        ) {
            tracing::debug!(target: "plan_frame", error = %e, "planframe.approved workflow event failed");
        }

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": plan.plan_id,
            "status": "approved",
            "version": plan.version,
            "grants_materialized": grants_materialized,
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct PlanFrameAmendTool;

impl NativeTool for PlanFrameAmendTool {
    fn name(&self) -> &'static str {
        "planframe_amend"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Amend an existing PlanFrame by creating a new immutable revision. The previous revision is preserved unchanged. If the prior revision was approved, the new revision INHERITS that approval unless the amendment expands the safety envelope (adds/removes a step, changes a step owner or agent, weakens/removes a validation gate, or broadens capability_envelope). Cosmetic changes (rewording objective/title, recording a progress reason) inherit automatically. Envelope-expanding changes re-open the operator gate. The response carries `diff_summary`, `inherited`, and `requires_regate` so the caller — and the operator — can see exactly what changed. The response also re-runs the `capability_preflight` (RFC #777 Part C) when any resolved step declares `required_capabilities`.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to amend"
                    },
                    "title": {
                        "type": "string",
                        "description": "Updated title (optional, defaults to current)"
                    },
                    "objective": {
                        "type": "string",
                        "description": "Updated objective (optional, defaults to current)"
                    },
                    "step_updates": {
                        "type": "array",
                        "description": "Progress-only partial updates for existing steps. Cannot change owner, agent_id, or depends_on — use steps for accountability changes. Inherits all unmentioned accountability fields from the current revision.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "title": { "type": "string" },
                                "notes": { "type": "string" },
                                "step_status": { "type": "string", "enum": ["pending", "in_progress", "completed", "failed", "skipped"], "description": "New execution status for this step" }
                            },
                            "required": ["step_id"],
                            "additionalProperties": false
                        }
                    },
                    "steps": {
                        "type": "array",
                        "description": "Complete replacement step list (optional, defaults to current). For existing step_ids, omitted per-step fields (title, owner, agent_id, depends_on, notes, step_status) inherit from the previous revision. Use step_updates for progress-only changes. 'status' is accepted as an alias for 'step_status'.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "title": { "type": "string" },
                                "owner": { "type": "string", "enum": ["planner", "agent", "operator", "shared"] },
                                "agent_id": { "type": "string" },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "notes": { "type": "string" },
                                "step_status": { "type": "string", "enum": ["pending", "in_progress", "completed", "failed", "skipped"], "description": "New execution status for this step" }
                            },
                            "required": ["step_id"]
                        }
                    },
                    "validation_policy": {
                        "type": "object",
                        "description": "Updated validation policy (optional, defaults to current)",
                        "properties": {
                            "entries": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "validation_id": { "type": "string" },
                                        "title": { "type": "string" },
                                        "class": { "type": "string", "enum": ["mechanical_safety", "security_review", "correctness_check", "quality_check", "packaging_check"] },
                                        "requirement": { "type": "string", "enum": ["required", "advisory"] }
                                    },
                                    "required": ["validation_id", "title"]
                                }
                            }
                        }
                    },
                    "capability_envelope": {
                        "type": "array",
                        "description": "Updated session capability envelope (optional, defaults to current)",
                        "items": { "type": "object" }
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason for the amendment"
                    }
                },
                "required": ["plan_id", "reason"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest) && can_perform(manifest, "planframe.amend")
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
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct StepInput {
            step_id: String,
            #[serde(default)]
            title: Option<String>,
            owner: Option<String>,
            agent_id: Option<String>,
            depends_on: Option<Vec<String>>,
            notes: Option<String>,
            required_capabilities: Option<Vec<String>>,
            #[serde(default, alias = "status")]
            step_status: Option<String>,
        }

        #[derive(Deserialize)]
        struct ValidationInput {
            validation_id: String,
            title: String,
            class: Option<String>,
            requirement: Option<String>,
        }

        #[derive(Deserialize)]
        struct ValidationPolicyInput {
            entries: Option<Vec<ValidationInput>>,
        }

        #[derive(Deserialize)]
        struct StepUpdateInput {
            step_id: String,
            title: Option<String>,
            notes: Option<String>,
            #[serde(default, alias = "status")]
            step_status: Option<String>,
        }

        #[derive(Deserialize)]
        struct Args {
            plan_id: String,
            title: Option<String>,
            objective: Option<String>,
            steps: Option<Vec<StepInput>>,
            step_updates: Option<Vec<StepUpdateInput>>,
            validation_policy: Option<ValidationPolicyInput>,
            capability_envelope: Option<Vec<serde_json::Value>>,
            reason: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        if args.steps.is_some() && args.step_updates.is_some() {
            return Ok(ToolError::validation(
                "Cannot use both `steps` and `step_updates` in the same amendment. Use `steps` to replace accountability (owner/agent_id/depends_on) and `step_updates` for progress-only changes.",
                Some("Provide exactly one of `steps` or `step_updates`, or neither."),
            )
            .with_code("plan_amend_conflicting_step_inputs")
            .to_error_response());
        }

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let Some(config) = config else {
            return Ok(ToolError::execution(
                "Gateway config not available",
                Some("Ensure the gateway configuration is loaded and valid."),
            )
            .with_code("gateway_config_unavailable")
            .to_error_response());
        };

        let Some(current) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(ToolError::not_found(
                "Plan",
                Some("Create a plan first or check the plan ID."),
            )
            .with_code("plan_not_found")
            .to_error_response());
        };

        if current.status == PlanStatus::Completed || current.status == PlanStatus::Cancelled {
            return Ok(ToolError::conflict(
                format!("Cannot amend a {} plan", current.status.as_str()),
                Some("Only plans in mutable status (not Completed or Cancelled) can be amended."),
            )
            .with_code("plan_wrong_status")
            .to_error_response());
        }

        let old_version = current.version;
        let new_version = old_version + 1;

        let steps = match (args.steps, args.step_updates) {
            (Some(steps), _) => {
                let previous_by_id: std::collections::HashMap<&str, &PlanStep> = current
                    .steps
                    .iter()
                    .map(|s| (s.step_id.as_str(), s))
                    .collect();

                // Validate step_status strings — reject typos early.
                const VALID_STATUSES: &[&str] =
                    &["pending", "in_progress", "completed", "failed", "skipped"];
                for s in &steps {
                    if let Some(ref ss) = s.step_status {
                        if !VALID_STATUSES.contains(&ss.as_str()) {
                            return Ok(ToolError::validation(
                                &format!(
                                    "Unknown step_status `{}` for step `{}`. Valid values: {}",
                                    ss,
                                    s.step_id,
                                    VALID_STATUSES.join(", "),
                                ),
                                Some("Use one of the valid step_status values."),
                            )
                            .with_code("invalid_step_status")
                            .to_error_response());
                        }
                    }
                }

                steps
                    .into_iter()
                    .map(|s| {
                        let prev = previous_by_id.get(s.step_id.as_str());
                        let owner = match s.owner.as_deref() {
                            Some("agent") => StepOwner::Agent,
                            Some("operator") => StepOwner::Operator,
                            Some("shared") => StepOwner::Shared,
                            Some("planner") => StepOwner::Planner,
                            _ => prev.map(|p| p.owner).unwrap_or(StepOwner::Agent),
                        };
                        let agent_id = s
                            .agent_id
                            .filter(|id| !id.trim().is_empty())
                            .or_else(|| prev.and_then(|p| p.agent_id.clone()));
                        let depends_on = match &s.depends_on {
                            Some(d) if !d.is_empty() => d.clone(),
                            _ => prev.map(|p| p.depends_on.clone()).unwrap_or_default(),
                        };
                        let notes = s.notes.or_else(|| prev.and_then(|p| p.notes.clone()));
                        let status = match s.step_status.as_deref() {
                            Some("in_progress") => StepStatus::InProgress,
                            Some("completed") => StepStatus::Completed,
                            Some("failed") => StepStatus::Failed,
                            Some("skipped") => StepStatus::Skipped,
                            Some("pending") => StepStatus::Pending,
                            _ => prev.map(|p| p.status).unwrap_or(StepStatus::Pending),
                        };
                        PlanStep {
                            step_id: s.step_id.clone(),
                            title: s.title.unwrap_or_else(|| {
                                prev.map(|p| p.title.clone())
                                    .unwrap_or_else(|| s.step_id.clone())
                            }),
                            owner,
                            depends_on,
                            agent_id,
                            notes,
                            status,
                            required_capabilities: s.required_capabilities.clone()
                                .unwrap_or_else(|| prev.map(|p| p.required_capabilities.clone()).unwrap_or_default()),
                        }
                    })
                    .collect::<Vec<_>>()
            }
            (_, Some(updates)) => {
                const VALID_STATUSES: &[&str] =
                    &["pending", "in_progress", "completed", "failed", "skipped"];
                for u in &updates {
                    if let Some(ref ss) = u.step_status {
                        if !VALID_STATUSES.contains(&ss.as_str()) {
                            return Ok(ToolError::validation(
                                &format!(
                                    "Unknown step_status `{}` for step `{}`. Valid values: {}",
                                    ss,
                                    u.step_id,
                                    VALID_STATUSES.join(", "),
                                ),
                                Some("Use one of the valid step_status values."),
                            )
                            .with_code("invalid_step_status")
                            .to_error_response());
                        }
                    }
                }

                let updates_by_id: std::collections::HashMap<&str, &StepUpdateInput> = updates
                    .iter()
                    .map(|u| (u.step_id.as_str(), u))
                    .collect();

                current
                    .steps
                    .iter()
                    .map(|prev| {
                        let u = updates_by_id.get(prev.step_id.as_str());
                        let title = u
                            .and_then(|u| u.title.clone())
                            .unwrap_or_else(|| prev.title.clone());
                        let notes = u
                            .and_then(|u| u.notes.clone())
                            .or_else(|| prev.notes.clone());
                        let status = u
                            .and_then(|u| u.step_status.as_deref())
                            .map(|ss| match ss {
                                "in_progress" => StepStatus::InProgress,
                                "completed" => StepStatus::Completed,
                                "failed" => StepStatus::Failed,
                                "skipped" => StepStatus::Skipped,
                                _ => StepStatus::Pending,
                            })
                            .unwrap_or(prev.status);
                        PlanStep {
                            step_id: prev.step_id.clone(),
                            title,
                            owner: prev.owner,
                            depends_on: prev.depends_on.clone(),
                            agent_id: prev.agent_id.clone(),
                            notes,
                            status,
                            required_capabilities: prev.required_capabilities.clone(),
                        }
                    })
                    .collect::<Vec<_>>()
            }
            (None, None) => current.steps.clone(),
        };

        // Whether at least one step's `status` actually changed between the
        // current revision and the resolved new one. Step-status transitions
        // (pending → completed) are the only cosmetic amend that constitutes
        // real progress; everything else cosmetic (reworded objective/title,
        // a new `reason` note) is bookkeeping. Surfaced on the result as
        // `progress_recorded` so the LoopGuard can treat status-only re-sends
        // as stagnant and stop amendment loops (observed in session-9d5b3ef1:
        // the planner re-sent the same single step 11 times, each amend
        // returned `ok: true`, and `register_progress` reset the no-progress
        // counter on every call).
        let prev_status: std::collections::HashMap<&str, StepStatus> = current
            .steps
            .iter()
            .map(|s| (s.step_id.as_str(), s.status))
            .collect();
        let step_status_changed = steps.iter().any(|s| {
            prev_status.get(s.step_id.as_str()).is_some_and(|ps| *ps != s.status)
        });

        // Validate the step dependency graph before persisting.
        if let Err(dag_err) = validate_step_dag(&steps) {
            return Ok(ToolError::validation(
                &dag_err,
                Some("Fix depends_on entries so they reference existing step_ids with no cycles."),
            )
            .with_code("invalid_step_dependencies")
            .to_error_response());
        }

        let validation_policy = match args.validation_policy {
            Some(vp) => ValidationPolicy {
                entries: vp
                    .entries
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| ValidationEntry {
                        validation_id: v.validation_id,
                        title: v.title,
                        class: match v.class.as_deref() {
                            Some("security_review") => {
                                autonoetic_types::plan_frame::ValidationClass::SecurityReview
                            }
                            Some("correctness_check") => {
                                autonoetic_types::plan_frame::ValidationClass::CorrectnessCheck
                            }
                            Some("quality_check") => {
                                autonoetic_types::plan_frame::ValidationClass::QualityCheck
                            }
                            Some("packaging_check") => {
                                autonoetic_types::plan_frame::ValidationClass::PackagingCheck
                            }
                            _ => autonoetic_types::plan_frame::ValidationClass::MechanicalSafety,
                        },
                        requirement: match v.requirement.as_deref() {
                            Some("advisory") => {
                                autonoetic_types::plan_frame::ValidationRequirement::Advisory
                            }
                            Some("waived") => {
                                autonoetic_types::plan_frame::ValidationRequirement::Waived
                            }
                            _ => autonoetic_types::plan_frame::ValidationRequirement::Required,
                        },
                    })
                    .collect(),
            },
            None => current.validation_policy.clone(),
        };

        let capability_envelope = if let Some(items) = args.capability_envelope {
            parse_capability_envelope_input(Some(items))?
        } else {
            current.capability_envelope.clone()
        };

        let now = now_rfc3339();

        // Guard: reject amendments that are semantically identical to the
        // current version. This prevents pointless version churn when the
        // LLM re-sends the same content (observed in planner amendment loops).
        // Compare the resolved fields (after inheritance) against current.
        let resolved_title = args.title.clone().unwrap_or_else(|| current.title.clone());
        let resolved_objective = args.objective.clone().unwrap_or_else(|| current.objective.clone());
        if steps == current.steps
            && resolved_title == current.title
            && resolved_objective == current.objective
            && validation_policy == current.validation_policy
            && capability_envelope == current.capability_envelope
        {
            return Ok(ToolError::validation(
                "Amendment is identical to the current version — no changes detected. Call planframe_get to see the current state, then make meaningful changes to steps, title, objective, validation policy, or capability envelope.",
                Some("Use planframe_get to check the current plan before attempting to amend."),
            )
            .with_code("plan_amend_identical")
            .to_error_response());
        }

        // Decide whether the amendment inherits the prior approval or
        // re-opens the operator gate. An envelope expansion (new/removed step,
        // owner/agent change, weakened/removed validation) re-gates; everything
        // else (objective rewording, title, progress reason) inherits. This is
        // a mechanical, gateway-computed classification — never LLM-judged —
        // and is more faithful to the constitution: the operator consents to
        // risky/irreversible change, not to progress bookkeeping.
        let envelope_diff = plan_envelope_diff(&current, &{
            // Build a transient child view to diff against, mirroring the
            // field resolution below (args override parent).
            let mut probe = current.clone();
            if let Some(t) = &args.title {
                probe.title = t.clone();
            }
            if let Some(o) = &args.objective {
                probe.objective = o.clone();
            }
            probe.steps = steps.clone();
            probe.validation_policy = validation_policy.clone();
            probe.capability_envelope = capability_envelope.clone();
            probe
        });
        let inherit = current.status == PlanStatus::Approved && envelope_diff.is_cosmetic_only();

        let new_revision = PlanFrame {
            plan_id: current.plan_id.clone(),
            version: new_version,
            parent_version: Some(old_version),
            workflow_id: current.workflow_id.clone(),
            root_session_id: current.root_session_id.clone(),
            title: args.title.unwrap_or(current.title.clone()),
            objective: args.objective.unwrap_or(current.objective.clone()),
            status: if inherit {
                PlanStatus::Approved
            } else {
                PlanStatus::AwaitingApproval
            },
            steps,
            validation_policy,
            capability_envelope,
            approved_by: if inherit {
                current.approved_by.clone()
            } else {
                None
            },
            approved_at: if inherit { Some(now.clone()) } else { None },
            created_by_agent_id: manifest.agent.id.clone(),
            reason: args.reason,
            created_at: now,
            expires_at: {
                let ttl = config.plan_frame_timeout_secs;
                if ttl == 0 {
                    None
                } else {
                    Some((chrono::Utc::now() + chrono::Duration::seconds(ttl as i64)).to_rfc3339())
                }
            },
        };

        store.save_plan_frame(&new_revision)?;

        // Keep the approval request aligned with the latest revision (#565).
        // Cosmetic amendments that inherit approval need no new gate. Envelope
        // changes (or amendments on a still-pending revision) open a new gate
        // for the new revision and supersede any still-pending gate for the old
        // revision (cancel approval, mark revision cancelled, emit plan.withdrawn).
        if !inherit {
            let session_id = _session_id.unwrap_or(&current.root_session_id);
            if current.status == PlanStatus::AwaitingApproval {
                supersede_pending_plan_revision(
                    &store,
                    &current.plan_id,
                    old_version,
                    new_version,
                    session_id,
                    &current.root_session_id,
                    &manifest.agent.id,
                );
            }
            let mut new_revision = new_revision.clone();
            match create_plan_approval_request(
                store.clone(),
                &new_revision,
                manifest,
                session_id,
                config,
                _turn_id,
            ) {
                Ok(PlanGateOutcome::Approved) => {
                    new_revision.status = PlanStatus::Approved;
                    new_revision.approved_by = Some("gate_service".to_string());
                    new_revision.approved_at = Some(now_rfc3339());
                    if let Err(e) = store.save_plan_frame(&new_revision) {
                        tracing::warn!(
                            target: "plan_frame",
                            error = %e,
                            plan_id = %new_revision.plan_id,
                            version = %new_revision.version,
                            "failed to mark gate-cleared amended plan as approved"
                        );
                    }
                }
                Ok(PlanGateOutcome::Pending) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "plan_frame",
                        error = %e,
                        plan_id = %new_revision.plan_id,
                        version = %new_revision.version,
                        "failed to create plan approval request after amend"
                    );
                }
            }
        }

        // Pillar C: revoke the prior approved plan's grants ONLY when the
        // envelope actually expanded. The condition is tighter than
        // `!inherit` (which also fires for non-cosmetic amends on a plan
        // that was never approved — there is no prior approved envelope
        // to withdraw). We need both: the parent was approved (so there
        // are materialized grants) AND the diff shows envelope expansion
        // (so those grants are stale). Inherited (cosmetic-only) and
        // envelope-equivalent amendments keep the existing grants.
        let grants_revoked = if current.status == PlanStatus::Approved
            && envelope_diff.requires_regate()
        {
            store
                .revoke_session_grants_by_source(
                    &current.root_session_id,
                    &current.plan_id,
                    "plan-amended (envelope expanded)",
                )
                .unwrap_or_else(|e| {
                    tracing::warn!(target: "plan_frame", error = %e, plan_id = %current.plan_id, "plan grant revoke failed");
                    0
                })
        } else {
            0
        };

        // Canonical timeline. An envelope-expanding amendment re-opens the
        // operator gate (plan.pending with the diff so the operator sees what
        // they are approving). A cosmetic amendment inherits and emits
        // plan.approved with `inherited: true` + the (empty) diff so the
        // checkpoint still surfaces and shows why the operator wasn't asked.
        {
            let root_session_id = new_revision.root_session_id.clone();
            let session_id = _session_id
                .map(str::to_string)
                .unwrap_or_else(|| root_session_id.clone());
            let role =
                crate::runtime::session_timeline::derive_role(&new_revision.created_by_agent_id);
            let principal = autonoetic_types::principal::Principal::agent(
                new_revision.created_by_agent_id.clone(),
            );
            let refs = autonoetic_types::session_timeline::TimelineRefs {
                plan_id: Some(new_revision.plan_id.clone()),
                ..Default::default()
            };
            // Both checkpoints carry the same triple (`inherited`,
            // `requires_regate`, `diff_summary`) so the timeline schema is
            // uniform and consumers don't have to infer a missing boolean.
            let (event_type, extra) = if inherit {
                (
                    "plan.approved",
                    serde_json::json!({
                        "inherited": true,
                        "requires_regate": envelope_diff.requires_regate(),
                        "inherited_from": old_version,
                        "diff_summary": envelope_diff.summary(),
                    }),
                )
            } else {
                (
                    "plan.pending",
                    serde_json::json!({
                        "inherited": false,
                        "requires_regate": envelope_diff.requires_regate(),
                        "diff_summary": envelope_diff.summary(),
                    }),
                )
            };
            let mut payload = serde_json::json!({
                "plan_id": new_revision.plan_id,
                "version": new_revision.version,
                "parent_version": new_revision.parent_version,
                "title": new_revision.title,
                "reason": new_revision.reason,
            });
            if let serde_json::Value::Object(map) = extra {
                for (k, v) in map {
                    payload[k] = v;
                }
            }
            let event = crate::runtime::session_timeline::build_timeline_event(
                root_session_id,
                session_id,
                _turn_id.map(str::to_string),
                &principal,
                &role,
                event_type,
                None,
                Some(payload),
                refs,
            );
            if let Err(e) = store.create_live_digest_event(&event) {
                tracing::debug!(target: "session_timeline", error = %e, "plan timeline emit failed (amend)");
            }
        }

        let wf = crate::scheduler::workflow_store::load_workflow_run(
            config,
            Some(&store),
            &current.workflow_id,
        )?;
        if let Some(mut wf) = wf {
            wf.active_plan_ref = Some(PlanRef {
                plan_id: current.plan_id.clone(),
                version: new_version,
            });
            wf.updated_at = now_rfc3339();
            crate::scheduler::workflow_store::save_workflow_run(config, Some(&store), &wf)?;
        }

        crate::scheduler::workflow_store::append_workflow_event(
            config,
            Some(&store),
            &autonoetic_types::workflow::WorkflowEventRecord {
                event_id: {
                    let bytes = uuid::Uuid::new_v4();
                    format!("evt-{}", hex::encode(&bytes.as_bytes()[..8]))
                },
                workflow_id: current.workflow_id.clone(),
                task_id: None,
                event_type: "planframe.amended".to_string(),
                agent_id: Some(manifest.agent.id.clone()),
                payload: serde_json::json!({
                    "plan_id": current.plan_id,
                    "old_version": old_version,
                    "new_version": new_version,
                    "title": new_revision.title,
                    "step_count": new_revision.steps.len(),
                    "step_titles": new_revision.steps.iter().map(|s| s.title.clone()).collect::<Vec<_>>(),
                    "reason": new_revision.reason,
                }),
                occurred_at: now_rfc3339(),
            },
        )?;

        // RFC #777 Part C — re-run the capability preflight on the amended
        // revision. Same advisory shape as `planframe_propose`: warnings
        // surface to the planner, never block. Re-runs on every amend so a
        // step that newly declares `required_capabilities`, or a step whose
        // `agent_id` was changed, gets fresh findings.
        let preflight_view = compute_preflight_view(config, &new_revision);

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": current.plan_id,
            "version": new_version,
            "status": if inherit { "approved" } else { "awaiting_approval" },
            "parent_version": old_version,
            "inherited": inherit,
            "diff_summary": envelope_diff.summary(),
            "requires_regate": envelope_diff.requires_regate(),
            "grants_revoked": grants_revoked,
            // True iff this amend recorded real plan progress (a step-status
            // transition) or expanded the envelope. False for cosmetic-only
            // amends that changed nothing but title/objective/reason text.
            // The LoopGuard uses this to avoid resetting the no-progress
            // counter on stagnant amendment loops.
            "progress_recorded": step_status_changed || envelope_diff.requires_regate(),
            "capability_preflight": preflight_view,
            "message": if inherit {
                "Plan amended (no envelope change) — operator approval inherited from the prior revision."
            } else {
                "Plan amended (envelope changed) — operator re-approval is required."
            },
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct PlanFrameHistoryTool;

impl NativeTool for PlanFrameHistoryTool {
    fn name(&self) -> &'static str {
        "planframe_history"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Get the full revision history of a plan. Returns all revisions from first to latest, showing how the plan evolved.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to retrieve history for"
                    }
                },
                "required": ["plan_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_plan_frame_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            plan_id: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::execution(
                "Gateway store not available",
                Some(
                    "Ensure the gateway database is initialized and the store path is accessible.",
                ),
            )
            .with_code("gateway_store_unavailable")
            .to_error_response());
        };

        let revisions = store.list_plan_revisions(&args.plan_id)?;

        if revisions.is_empty() {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": true,
                "plan_id": args.plan_id,
                "revisions": [],
                "message": "No revisions found for this plan"
            }))?);
        }

        let summaries: Vec<PlanFrameSummary> =
            revisions.iter().map(|r| r.compact_summary()).collect();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": args.plan_id,
            "revisions": summaries,
            "count": summaries.len(),
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    fn pats(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // The planner holds `PlanFrameAccess: ["*"]`. The wildcard must grant
    // participation but NOT the authority to approve — otherwise the proposer
    // can self-approve its own plan (separation of powers).
    #[test]
    fn wildcard_grants_participation_not_approval_authority() {
        let p = pats(&["*"]);
        assert!(patterns_allow(&p, "planframe.propose"));
        assert!(patterns_allow(&p, "planframe.amend"));
        assert!(!patterns_allow(&p, "planframe.approve"));
    }

    // A prefix pattern also must not confer the approve authority.
    #[test]
    fn prefix_pattern_does_not_grant_approval_authority() {
        let p = pats(&["planframe."]);
        assert!(patterns_allow(&p, "planframe.propose"));
        assert!(!patterns_allow(&p, "planframe.approve"));
    }

    // An authority is granted only by an exact `planframe.approve` right.
    #[test]
    fn explicit_grant_confers_approval_authority() {
        assert!(patterns_allow(
            &pats(&["planframe.approve"]),
            "planframe.approve"
        ));
        assert!(patterns_allow(
            &pats(&["planframe.propose", "planframe.approve"]),
            "planframe.approve"
        ));
    }

    // Empty / whitespace / degenerate-prefix patterns must grant nothing —
    // otherwise `starts_with("")` silently authorizes every participation op.
    #[test]
    fn empty_and_degenerate_patterns_grant_nothing() {
        for bad in [&[""][..], &["   "][..], &["."][..]] {
            let p: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(!patterns_allow(&p, "planframe.propose"), "pattern {bad:?}");
            assert!(!patterns_allow(&p, "planframe.approve"), "pattern {bad:?}");
        }
    }

    #[test]
    fn approve_is_the_authority_operation() {
        use autonoetic_types::capability::AuthorityOp;
        assert!(AuthorityOp::is_authority_operation("planframe.approve"));
        assert!(!AuthorityOp::is_authority_operation("planframe.propose"));
        assert!(!AuthorityOp::is_authority_operation("planframe.amend"));
    }
}
