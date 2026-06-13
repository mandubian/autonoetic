use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{
    plan_envelope_diff, PlanFrame, PlanFrameSummary, PlanRef, PlanStatus, PlanStep, StepOwner,
    ValidationEntry, ValidationPolicy,
};
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
/// The envelope is derived MECHANICALLY from each plan step's `agent_id` →
/// declared `Capability::NetworkAccess.hosts` (never LLM-judged). Wildcards
/// (`"*"`) are skipped because they don't materialize to a concrete,
/// matchable grant and would defeat the dedup's concreteness rule (the exec
/// cache only auto-approves when all patterns are `url_literal`/`ip_address`).
///
/// Best-effort: any failure (missing config, agent not installed, DB error)
/// returns 0 and the approval still succeeds. The grant carries
/// `source_approval_id = Some(plan_id)` so a later envelope-expanding amend
/// can revoke it surgically via `revoke_session_grants_by_source`.
///
/// Returns the number of concrete hosts materialized.
fn materialize_plan_grants(
    store: &crate::scheduler::gateway_store::GatewayStore,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    plan: &PlanFrame,
    approver: &str,
    now: &str,
) -> usize {
    let Some(config) = config else { return 0 };
    let repo = crate::AgentRepository::from_config(config);
    let mut hosts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for step in &plan.steps {
        let Some(agent_id) = step.agent_id.as_deref() else { continue };
        let loaded = match repo.get_sync(agent_id) {
            Ok(l) => l,
            Err(_) => continue, // not-yet-installed agent — its tool calls go through normal approval
        };
        for cap in &loaded.manifest.capabilities {
            if let autonoetic_types::capability::Capability::NetworkAccess { hosts: decl } = cap {
                for h in decl {
                    if h == "*" { continue; }
                    hosts.insert(h.clone());
                }
            }
        }
    }
    if hosts.is_empty() {
        return 0;
    }
    let targets: Vec<autonoetic_types::background::GrantTarget> = hosts
        .iter()
        .map(|h| autonoetic_types::background::GrantTarget::ExactHost(h.clone()))
        .collect();
    if let Err(e) = store.insert_session_grant(
        &plan.root_session_id,
        &plan.root_session_id,
        &plan.created_by_agent_id,
        &autonoetic_types::background::GrantScope::RootSession,
        &targets,
        approver,
        now,
        Some(&plan.plan_id),
        None,
    ) {
        tracing::warn!(target: "plan_frame", error = %e, plan_id = %plan.plan_id, "plan grant materialization failed");
        return 0;
    }
    hosts.len()
}

fn has_plan_frame_access(manifest: &AgentManifest) -> bool {
    manifest.capabilities.iter().any(|c| {
        matches!(c, Capability::PlanFrameAccess { .. })
    })
}

fn can_perform(manifest: &AgentManifest, operation: &str) -> bool {
    manifest.capabilities.iter().any(|c| {
        match c {
            Capability::PlanFrameAccess { patterns } => patterns
                .iter()
                .any(|p| p == "*" || p == operation || operation.starts_with(p.trim_end_matches('.'))),
            _ => false,
        }
    })
}

fn new_plan_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("plan-{}", hex::encode(&bytes.as_bytes()[..6]))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub struct PlanFrameProposeTool;

impl NativeTool for PlanFrameProposeTool {
    fn name(&self) -> &'static str {
        "planframe_propose"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Propose a new PlanFrame for collaborative work. Creates a workflow if one does not exist yet. The plan starts in 'awaiting_approval' status and must be approved before agents act on it.".to_string(),
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
                                "notes": { "type": "string" }
                            },
                            "required": ["step_id", "title"]
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
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
        };

        let Some(config) = config else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway config not available"
            }))?);
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
                            Some("security_review") => autonoetic_types::plan_frame::ValidationClass::SecurityReview,
                            Some("correctness_check") => autonoetic_types::plan_frame::ValidationClass::CorrectnessCheck,
                            Some("quality_check") => autonoetic_types::plan_frame::ValidationClass::QualityCheck,
                            Some("packaging_check") => autonoetic_types::plan_frame::ValidationClass::PackagingCheck,
                            _ => autonoetic_types::plan_frame::ValidationClass::MechanicalSafety,
                        },
                        requirement: match v.requirement.as_deref() {
                            Some("advisory") => autonoetic_types::plan_frame::ValidationRequirement::Advisory,
                            Some("waived") => autonoetic_types::plan_frame::ValidationRequirement::Waived,
                            _ => autonoetic_types::plan_frame::ValidationRequirement::Required,
                        },
                    })
                    .collect(),
            },
            None => ValidationPolicy::default(),
        };

        let plan = PlanFrame {
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
            approved_by: None,
            approved_at: None,
            created_by_agent_id: manifest.agent.id.clone(),
            reason: None,
            created_at: now,
        };

        store.save_plan_frame(&plan)?;

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
        crate::scheduler::workflow_store::save_workflow_run(config, Some(&store), &updated_workflow)?;

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

        let summary = plan.compact_summary();
        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": plan_id,
            "workflow_id": updated_workflow.workflow_id,
            "status": "awaiting_approval",
            "version": 1,
            "message": "Plan proposed. Operator approval is required before agents can act on it.",
            "summary": summary,
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
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
        };

        let plan = if let Some(pid) = &args.plan_id {
            if let Some(ver) = args.version {
                store.load_plan_frame_revision(pid, ver)?
            } else {
                store.load_plan_frame(pid)?
            }
        } else {
            let sid = session_id.ok_or_else(|| anyhow::anyhow!("session_id required when plan_id not specified"))?;
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
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
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
            description: "Approve a PlanFrame. Moves status from 'awaiting_approval' to 'approved'. Typically invoked by the operator through the gateway, not directly by agents.".to_string(),
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
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
        };

        let Some(plan) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Plan not found"
            }))?);
        };

        if plan.status != PlanStatus::AwaitingApproval {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("Plan is in '{}' status; only awaiting_approval plans can be approved", plan.status.as_str())
            }))?);
        }

        let now = now_rfc3339();
        let approver = args.approved_by.unwrap_or_else(|| manifest.agent.id.clone());

        store.update_plan_frame_status(
            &plan.plan_id,
            plan.version,
            PlanStatus::Approved,
            Some(&approver),
            Some(&now),
        )?;

        // Canonical timeline: the plan gate closes (#363 P1), authored by the
        // approver (Operator seat for a human, the agent's seat otherwise).
        {
            use autonoetic_types::session_timeline::TimelineRefs;
            let (principal, role) = crate::runtime::session_timeline::decider_seat(&approver);
            let refs = TimelineRefs {
                plan_id: Some(plan.plan_id.clone()),
                ..Default::default()
            };
            let event = crate::runtime::session_timeline::build_timeline_event(
                plan.root_session_id.clone(),
                plan.root_session_id.clone(),
                None,
                &principal,
                &role,
                "plan.approved",
                None,
                Some(serde_json::json!({
                    "plan_id": plan.plan_id,
                    "version": plan.version,
                    "approved_by": approver,
                })),
                refs,
            );
            if let Err(e) = store.create_live_digest_event(&event) {
                tracing::debug!(target: "session_timeline", error = %e, "plan.approved timeline emit failed");
            }
        }

        // Pillar C: materialize the plan's declared network envelope as a
        // session approval grant. Best-effort — never blocks the approval.
        let grants_materialized = materialize_plan_grants(&store, config, &plan, &approver, &now);

        if let Some(config) = config {
            crate::scheduler::workflow_store::append_workflow_event(
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
            )?;
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
            description: "Amend an existing PlanFrame by creating a new immutable revision. The previous revision is preserved unchanged. If the prior revision was approved, the new revision INHERITS that approval unless the amendment expands the safety envelope (adds/removes a step, changes a step owner or agent, or weakens/removes a validation gate). Cosmetic changes (rewording objective/title, recording a progress reason) inherit automatically. Envelope-expanding changes re-open the operator gate. The response carries `diff_summary`, `inherited`, and `requires_regate` so the caller — and the operator — can see exactly what changed.".to_string(),
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
                    "steps": {
                        "type": "array",
                        "description": "Complete replacement step list (optional, defaults to current)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "title": { "type": "string" },
                                "owner": { "type": "string", "enum": ["planner", "agent", "operator", "shared"] },
                                "agent_id": { "type": "string" },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "notes": { "type": "string" }
                            },
                            "required": ["step_id", "title"]
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
            title: String,
            owner: Option<String>,
            agent_id: Option<String>,
            depends_on: Option<Vec<String>>,
            notes: Option<String>,
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
            plan_id: String,
            title: Option<String>,
            objective: Option<String>,
            steps: Option<Vec<StepInput>>,
            validation_policy: Option<ValidationPolicyInput>,
            reason: Option<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
        };

        let Some(current) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Plan not found"
            }))?);
        };

        if current.status == PlanStatus::Completed || current.status == PlanStatus::Cancelled {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("Cannot amend a {} plan", current.status.as_str())
            }))?);
        }

        let old_version = current.version;
        let new_version = old_version + 1;

        let steps = match args.steps {
            Some(steps) => {
                let previous_by_id: std::collections::HashMap<&str, &PlanStep> = current
                    .steps
                    .iter()
                    .map(|s| (s.step_id.as_str(), s))
                    .collect();
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
                            _ => prev
                                .map(|p| p.depends_on.clone())
                                .unwrap_or_default(),
                        };
                        let notes = s.notes.or_else(|| prev.and_then(|p| p.notes.clone()));
                        PlanStep {
                            step_id: s.step_id,
                            title: s.title,
                            owner,
                            depends_on,
                            agent_id,
                            notes,
                        }
                    })
                    .collect()
            }
            None => current.steps.clone(),
        };

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
                            Some("security_review") => autonoetic_types::plan_frame::ValidationClass::SecurityReview,
                            Some("correctness_check") => autonoetic_types::plan_frame::ValidationClass::CorrectnessCheck,
                            Some("quality_check") => autonoetic_types::plan_frame::ValidationClass::QualityCheck,
                            Some("packaging_check") => autonoetic_types::plan_frame::ValidationClass::PackagingCheck,
                            _ => autonoetic_types::plan_frame::ValidationClass::MechanicalSafety,
                        },
                        requirement: match v.requirement.as_deref() {
                            Some("advisory") => autonoetic_types::plan_frame::ValidationRequirement::Advisory,
                            Some("waived") => autonoetic_types::plan_frame::ValidationRequirement::Waived,
                            _ => autonoetic_types::plan_frame::ValidationRequirement::Required,
                        },
                    })
                    .collect(),
            },
            None => current.validation_policy.clone(),
        };

        let now = now_rfc3339();

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
            probe
        });
        let inherit = current.status == PlanStatus::Approved
            && envelope_diff.is_cosmetic_only();

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
            approved_by: if inherit {
                current.approved_by.clone()
            } else {
                None
            },
            approved_at: if inherit { Some(now.clone()) } else { None },
            created_by_agent_id: manifest.agent.id.clone(),
            reason: args.reason,
            created_at: now,
        };

        store.save_plan_frame(&new_revision)?;

        // Pillar C: when the envelope expands (re-gate), the grants materialized
        // from the prior approved revision are stale (the new approval may
        // declare a different agent set / different network envelope). Revoke
        // them by source so the next approval re-materializes a clean grant.
        // Inherited (cosmetic-only) amendments keep the existing grants — the
        // envelope didn't change.
        let grants_revoked = if !inherit {
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

        if let Some(config) = config {
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
        }

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
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Gateway store not available"
            }))?);
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

        let summaries: Vec<PlanFrameSummary> = revisions.iter().map(|r| r.compact_summary()).collect();

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
