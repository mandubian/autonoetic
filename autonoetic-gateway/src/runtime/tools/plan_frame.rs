use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::{AgentManifest, ToolTier};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{
    PlanFrame, PlanFrameSummary, PlanRef, PlanStatus, PlanStep, StepOwner, StepStatus,
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
            description: "Propose a new PlanFrame for collaborative work. Creates a workflow if one does not exist yet. The plan starts in 'draft' status and must be approved before agents act on it.".to_string(),
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
                status: StepStatus::Pending,
                depends_on: s.depends_on.unwrap_or_default(),
                task_ids: vec![],
                artifact_refs: vec![],
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
            workflow_id: workflow.workflow_id.clone(),
            root_session_id: root_session_id.to_string(),
            title: args.title,
            objective: args.objective,
            status: PlanStatus::AwaitingApproval,
            version: 1,
            steps,
            validation_policy,
            approved_by: None,
            approved_at: None,
            created_by_agent_id: manifest.agent.id.clone(),
            updated_at: now.clone(),
            created_at: now,
        };

        store.save_plan_frame(&plan)?;

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
            description: "Get a PlanFrame by plan_id, or get the active plan for the current session. Returns the full plan including steps and validation policy.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to retrieve. Omit to get the active plan for the current workflow."
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
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            plan_id: Option<String>,
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
            store.load_plan_frame(pid)?
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
                    Ok(serde_json::to_string(&serde_json::json!({
                        "ok": true,
                        "plan": p,
                    }))?)
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
            description: "List all PlanFrames for the current workflow. Returns compact summaries.".to_string(),
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
        arguments_json: &str,
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
                        "description": "The plan ID to approve"
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

        let Some(mut plan) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Plan not found"
            }))?);
        };

        if plan.status != PlanStatus::AwaitingApproval && plan.status != PlanStatus::Draft {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("Plan is in '{}' status; only draft or awaiting_approval plans can be approved", plan.status.as_str())
            }))?);
        }

        let now = now_rfc3339();
        plan.status = PlanStatus::Approved;
        plan.approved_by = Some(args.approved_by.unwrap_or_else(|| manifest.agent.id.clone()));
        plan.approved_at = Some(now.clone());
        plan.updated_at = now.clone();

        store.save_plan_frame(&plan)?;

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
            description: "Amend an existing PlanFrame. Increments the version. Substantive changes (scope, validation, risk) should require operator re-approval.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan_id": {
                        "type": "string",
                        "description": "The plan ID to amend"
                    },
                    "title": {
                        "type": "string",
                        "description": "Updated title (optional)"
                    },
                    "objective": {
                        "type": "string",
                        "description": "Updated objective (optional)"
                    },
                    "steps": {
                        "type": "array",
                        "description": "Complete replacement step list (optional)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "title": { "type": "string" },
                                "owner": { "type": "string", "enum": ["planner", "agent", "operator", "shared"] },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "skipped", "blocked"] },
                                "agent_id": { "type": "string" },
                                "depends_on": { "type": "array", "items": { "type": "string" } },
                                "notes": { "type": "string" }
                            },
                            "required": ["step_id", "title"]
                        }
                    },
                    "step_updates": {
                        "type": "array",
                        "description": "Partial updates to specific steps (optional)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_id": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "skipped", "blocked"] },
                                "task_ids": { "type": "array", "items": { "type": "string" } },
                                "artifact_refs": { "type": "array", "items": { "type": "string" } },
                                "notes": { "type": "string" }
                            },
                            "required": ["step_id"]
                        }
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason for the amendment"
                    }
                },
                "required": ["plan_id"],
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
            status: Option<String>,
            agent_id: Option<String>,
            depends_on: Option<Vec<String>>,
            notes: Option<String>,
        }

        #[derive(Deserialize)]
        struct StepUpdate {
            step_id: String,
            status: Option<String>,
            task_ids: Option<Vec<String>>,
            artifact_refs: Option<Vec<String>>,
            notes: Option<String>,
        }

        #[derive(Deserialize)]
        struct Args {
            plan_id: String,
            title: Option<String>,
            objective: Option<String>,
            steps: Option<Vec<StepInput>>,
            step_updates: Option<Vec<StepUpdate>>,
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

        let Some(mut plan) = store.load_plan_frame(&args.plan_id)? else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": "Plan not found"
            }))?);
        };

        if plan.status == PlanStatus::Completed || plan.status == PlanStatus::Cancelled {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("Cannot amend a {} plan", plan.status.as_str())
            }))?);
        }

        let old_version = plan.version;
        plan.version += 1;

        if let Some(title) = args.title {
            plan.title = title;
        }
        if let Some(objective) = args.objective {
            plan.objective = objective;
        }

        if let Some(steps) = args.steps {
            plan.steps = steps
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
                    status: match s.status.as_deref() {
                        Some("in_progress") => StepStatus::InProgress,
                        Some("completed") => StepStatus::Completed,
                        Some("skipped") => StepStatus::Skipped,
                        Some("blocked") => StepStatus::Blocked,
                        _ => StepStatus::Pending,
                    },
                    depends_on: s.depends_on.unwrap_or_default(),
                    task_ids: vec![],
                    artifact_refs: vec![],
                    agent_id: s.agent_id,
                    notes: s.notes,
                })
                .collect();
        }

        if let Some(updates) = args.step_updates {
            for upd in updates {
                if let Some(step) = plan.steps.iter_mut().find(|s| s.step_id == upd.step_id) {
                    if let Some(status) = upd.status {
                        step.status = match status.as_str() {
                            "in_progress" => StepStatus::InProgress,
                            "completed" => StepStatus::Completed,
                            "skipped" => StepStatus::Skipped,
                            "blocked" => StepStatus::Blocked,
                            _ => StepStatus::Pending,
                        };
                    }
                    if let Some(task_ids) = upd.task_ids {
                        step.task_ids = task_ids;
                    }
                    if let Some(artifact_refs) = upd.artifact_refs {
                        step.artifact_refs = artifact_refs;
                    }
                    if let Some(notes) = upd.notes {
                        step.notes = Some(notes);
                    }
                }
            }
        }

        let now = now_rfc3339();
        plan.updated_at = now.clone();

        if plan.status == PlanStatus::Approved {
            plan.status = PlanStatus::AwaitingApproval;
        }

        store.save_plan_frame(&plan)?;

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
                    event_type: "planframe.amended".to_string(),
                    agent_id: Some(manifest.agent.id.clone()),
                    payload: serde_json::json!({
                        "plan_id": plan.plan_id,
                        "old_version": old_version,
                        "new_version": plan.version,
                        "reason": args.reason,
                    }),
                    occurred_at: now,
                },
            )?;
        }

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "plan_id": plan.plan_id,
            "version": plan.version,
            "status": plan.status.as_str(),
            "message": if plan.status == PlanStatus::AwaitingApproval {
                "Plan amended. Operator re-approval is required."
            } else {
                "Plan amended."
            },
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}
