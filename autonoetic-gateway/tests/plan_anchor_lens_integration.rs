mod support;

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration, ScriptInputMode};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{
    PlanFrame, PlanStatus, PlanStep, StepOwner, ValidationClass, ValidationEntry,
    ValidationPolicy, ValidationRequirement,
};
use tempfile::tempdir;

fn planner_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "planner.collaborative".to_string(),
            name: "Collaborative Planner".to_string(),
            description: "Test planner".to_string(),
        },
        capabilities: vec![
            Capability::ReadAccess { scopes: vec!["*".to_string()] },
            Capability::WriteAccess { scopes: vec!["*".to_string()] },
            Capability::PlanFrameAccess { patterns: vec!["*".to_string()] },
        ],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        execution_mode: ExecutionMode::default(),
        script_entry: None,
        script_input_mode: ScriptInputMode::default(),
        gateway_url: None,
        gateway_token: None,
        middleware: None,
        agentskills_import: None,
        allowed_tool_tiers: vec![],
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn make_plan(workflow_id: &str, root_session: &str) -> PlanFrame {
    PlanFrame {
        plan_id: format!("plan_{}", workflow_id),
        version: 1,
        parent_version: None,
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session.to_string(),
        title: "Add OAuth login".to_string(),
        objective: "Replace legacy auth with OAuth".to_string(),
        status: PlanStatus::Approved,
        steps: vec![
            PlanStep {
                step_id: "op_login".to_string(),
                title: "Operator designs login flow".to_string(),
                owner: StepOwner::Operator,
                depends_on: vec![],
                agent_id: None,
                notes: None,
            },
            PlanStep {
                step_id: "agent_oauth".to_string(),
                title: "Agent implements OAuth".to_string(),
                owner: StepOwner::Agent,
                depends_on: vec!["op_login".to_string()],
                agent_id: None,
                notes: None,
            },
        ],
        validation_policy: ValidationPolicy {
            entries: vec![
                ValidationEntry {
                    validation_id: "unit_tests".to_string(),
                    title: "Unit Tests".to_string(),
                    class: ValidationClass::CorrectnessCheck,
                    requirement: ValidationRequirement::Advisory,
                },
                ValidationEntry {
                    validation_id: "security_review".to_string(),
                    title: "Security Review".to_string(),
                    class: ValidationClass::SecurityReview,
                    requirement: ValidationRequirement::Required,
                },
            ],
        },
        capability_envelope: Vec::new(),
        approved_by: Some("operator".to_string()),
        approved_at: Some("2026-06-01T00:00:00Z".to_string()),
        created_by_agent_id: "planner.collaborative".to_string(),
        reason: Some("initial draft".to_string()),
        created_at: "2026-06-01T00:00:00Z".to_string(),
    }
}

fn bootstrap_workflow_and_plan(
    config: &GatewayConfig,
    store: &Arc<GatewayStore>,
    manifest: &AgentManifest,
    session_id: &str,
) -> String {
    let _ = workflow_store::ensure_workflow_for_root_session(
        config,
        Some(store),
        session_id,
        Some(&manifest.agent.id),
    )
    .unwrap();
    let workflow_id = workflow_store::resolve_workflow_id_for_root_session(config, session_id)
        .ok()
        .flatten()
        .expect("workflow id");
    let plan = make_plan(&workflow_id, session_id);
    store.save_plan_frame(&plan).unwrap();
    workflow_id
}

#[test]
fn plan_anchor_loads_from_store_with_expected_summary() {
    let dir = tempdir().unwrap();
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.path().to_path_buf();
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let _registry = default_registry();
    let manifest = planner_manifest();
    let _policy = PolicyEngine::new(manifest.clone());
    let _agent_dir = dir.path().join("planner.collaborative");
    let _cs = ContentStore::new(&gateway_dir).unwrap();

    let session_id = "root-session-plan-anchor-1";
    let workflow_id = bootstrap_workflow_and_plan(&config, &store, &manifest, session_id);

    let loaded = store
        .load_active_plan_for_workflow(&workflow_id)
        .unwrap()
        .expect("plan should load");
    let summary = loaded.compact_summary();

    assert_eq!(summary.plan_id, loaded.plan_id);
    assert_eq!(summary.version, 1);
    assert_eq!(summary.title, "Add OAuth login");
    assert_eq!(summary.step_count, 2);
    assert_eq!(summary.operator_steps, vec!["op_login".to_string()]);
    assert_eq!(summary.agent_steps, vec!["agent_oauth".to_string()]);
    assert_eq!(summary.required_validations, vec!["security_review".to_string()]);
    assert_eq!(summary.advisory_validations, vec!["unit_tests".to_string()]);
}

#[test]
fn plan_anchor_missing_when_workflow_has_no_plan() {
    let dir = tempdir().unwrap();
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.path().to_path_buf();
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let _registry = default_registry();
    let manifest = planner_manifest();
    let _policy = PolicyEngine::new(manifest.clone());
    let _cs = ContentStore::new(&gateway_dir).unwrap();

    let session_id = "root-session-plan-anchor-2";
    let _ = workflow_store::ensure_workflow_for_root_session(
        &config,
        Some(&store),
        session_id,
        Some(&manifest.agent.id),
    )
    .unwrap();
    let workflow_id = workflow_store::resolve_workflow_id_for_root_session(&config, session_id)
        .ok()
        .flatten()
        .expect("workflow id");

    let loaded = store.load_active_plan_for_workflow(&workflow_id).unwrap();
    assert!(loaded.is_none(), "no plan saved, so load should return None");
}

#[test]
fn plan_anchor_awaiting_approval_is_loaded() {
    // Both 'awaiting_approval' and 'approved' plans should be loaded
    // as the "active" plan, so the LLM can see the plan even before
    // operator approval has been recorded.
    let dir = tempdir().unwrap();
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.path().to_path_buf();
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let _registry = default_registry();
    let manifest = planner_manifest();
    let _policy = PolicyEngine::new(manifest.clone());
    let _cs = ContentStore::new(&gateway_dir).unwrap();

    let session_id = "root-session-plan-anchor-3";
    let _ = workflow_store::ensure_workflow_for_root_session(
        &config,
        Some(&store),
        session_id,
        Some(&manifest.agent.id),
    )
    .unwrap();
    let workflow_id = workflow_store::resolve_workflow_id_for_root_session(&config, session_id)
        .ok()
        .flatten()
        .expect("workflow id");
    let mut plan = make_plan(&workflow_id, session_id);
    plan.status = PlanStatus::AwaitingApproval;
    store.save_plan_frame(&plan).unwrap();

    let loaded = store
        .load_active_plan_for_workflow(&workflow_id)
        .unwrap()
        .expect("awaiting_approval plan should load");
    assert_eq!(loaded.status, PlanStatus::AwaitingApproval);
}
