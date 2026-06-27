//! Integration tests for plan step `depends_on` enforcement (#664).
//!
//! Three layers:
//! 1. DAG validation at propose/amend (rejects cycles, missing refs, self-deps)
//! 2. StepStatus tracking via planframe_amend (step_status field)
//! 3. unsatisfied_dependencies logic (the core check used by agent_spawn enforcement)

mod support;

use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration, ScriptInputMode};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{
    validate_step_dag, PlanFrame, PlanStatus, PlanStep, StepOwner, StepStatus,
};
use serde_json::json;
use tempfile::{tempdir, TempDir};

fn plan_frame_manifest() -> AgentManifest {
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
            Capability::AgentSpawn {
                max_children: 10,
                max_spawn_depth: 0,
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::WriteAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::PlanFrameAccess {
                patterns: vec!["*".to_string(), "planframe.approve".to_string()],
            },
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

fn make_config(dir: &std::path::Path) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.to_path_buf();
    config
}

fn mk_step(id: &str, deps: &[&str]) -> PlanStep {
    PlanStep {
        step_id: id.into(),
        title: format!("step {id}"),
        owner: StepOwner::Agent,
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        agent_id: None,
        notes: None,
        status: StepStatus::Pending,
    }
}

// ---------------------------------------------------------------------------
// Layer 1: DAG validation — unit tests
// ---------------------------------------------------------------------------

#[test]
fn validate_dag_accepts_valid_linear_chain() {
    let steps = vec![
        mk_step("s1", &[]),
        mk_step("s2", &["s1"]),
        mk_step("s3", &["s2"]),
    ];
    assert!(validate_step_dag(&steps).is_ok());
}

#[test]
fn validate_dag_accepts_empty_steps() {
    assert!(validate_step_dag(&[]).is_ok());
}

#[test]
fn validate_dag_rejects_self_dependency() {
    let steps = vec![mk_step("s1", &["s1"])];
    let err = validate_step_dag(&steps).unwrap_err();
    assert!(err.contains("depends on itself"), "got: {err}");
}

#[test]
fn validate_dag_rejects_missing_reference() {
    let steps = vec![mk_step("s1", &["nonexistent"])];
    let err = validate_step_dag(&steps).unwrap_err();
    assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn validate_dag_rejects_cycle() {
    let steps = vec![
        mk_step("s1", &["s3"]),
        mk_step("s2", &["s1"]),
        mk_step("s3", &["s2"]),
    ];
    let err = validate_step_dag(&steps).unwrap_err();
    assert!(err.contains("cycle"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Layer 1: DAG validation — propose tool rejects bad plans
// ---------------------------------------------------------------------------

fn execute_propose(
    dir: &TempDir,
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &autonoetic_gateway::policy::PolicyEngine,
    config: &GatewayConfig,
    store: &std::sync::Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    steps: &[serde_json::Value],
) -> serde_json::Value {
    let gateway_dir = dir.path().join(".gateway");
    let args = json!({
        "title": "Test plan",
        "objective": "Test objective",
        "steps": steps,
    });
    let result = registry
        .execute(
            "planframe_propose",
            manifest,
            policy,
            dir.path(),
            Some(&gateway_dir),
            &args.to_string(),
            Some(session_id),
            Some("turn-1"),
            Some(config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    serde_json::from_str(&result).unwrap()
}

#[test]
fn planframe_propose_rejects_cycle() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let registry = default_registry();
    let manifest = plan_frame_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let result = execute_propose(
        &dir, &registry, &manifest, &policy, &config, &store, "root-001/planner-001",
        &[
            json!({"step_id": "s1", "title": "A", "depends_on": ["s3"]}),
            json!({"step_id": "s3", "title": "C", "depends_on": ["s1"]}),
        ],
    );
    assert_eq!(result["ok"], false);
    assert!(result["message"].as_str().unwrap().contains("cycle"));
}

#[test]
fn planframe_propose_rejects_missing_dependency_reference() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let registry = default_registry();
    let manifest = plan_frame_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let result = execute_propose(
        &dir, &registry, &manifest, &policy, &config, &store, "root-002/planner-001",
        &[json!({"step_id": "s1", "title": "A", "depends_on": ["ghost"]})],
    );
    assert_eq!(result["ok"], false);
    assert!(result["message"].as_str().unwrap().contains("does not exist"));
}

#[test]
fn planframe_propose_accepts_valid_dag() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let registry = default_registry();
    let manifest = plan_frame_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let result = execute_propose(
        &dir, &registry, &manifest, &policy, &config, &store, "root-003/planner-001",
        &[
            json!({"step_id": "s1", "title": "Design", "owner": "agent", "agent_id": "architect.default"}),
            json!({"step_id": "s2", "title": "Implement", "owner": "agent", "agent_id": "coder.default", "depends_on": ["s1"]}),
            json!({"step_id": "s3b", "title": "Package deps", "owner": "agent", "agent_id": "packager.default", "depends_on": ["s2"]}),
            json!({"step_id": "s4", "title": "Federation gates", "owner": "planner", "depends_on": ["s3b"]}),
        ],
    );
    assert_eq!(result["ok"], true, "propose should succeed: {result}");
    assert_eq!(result["status"], "awaiting_approval");
}

// ---------------------------------------------------------------------------
// Layer 2: StepStatus tracking via planframe_amend
// ---------------------------------------------------------------------------

fn propose_and_approve(
    dir: &TempDir,
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &autonoetic_gateway::policy::PolicyEngine,
    config: &GatewayConfig,
    store: &std::sync::Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    steps: &[serde_json::Value],
) -> String {
    let result = execute_propose(dir, registry, manifest, policy, config, store, session_id, steps);
    let plan_id = result["plan_id"].as_str().unwrap().to_string();

    let gateway_dir = dir.path().join(".gateway");
    let approve_args = json!({"plan_id": &plan_id});
    registry
        .execute(
            "planframe_approve",
            manifest,
            policy,
            dir.path(),
            Some(&gateway_dir),
            &approve_args.to_string(),
            Some(session_id),
            Some("turn-1"),
            Some(config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    plan_id
}

#[test]
fn planframe_amend_sets_step_status_completed() {
    let dir = tempdir().unwrap();
    let config = make_config(dir.path());
    let registry = default_registry();
    let manifest = plan_frame_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let session_id = "root-amend-001/planner-001";
    let plan_id = propose_and_approve(
        &dir, &registry, &manifest, &policy, &config, &store, session_id,
        &[
            json!({"step_id": "s1", "title": "Step 1", "owner": "agent"}),
            json!({"step_id": "s2", "title": "Step 2", "owner": "agent", "depends_on": ["s1"]}),
        ],
    );

    // Amend to mark s1 as completed
    let amend_args = json!({
        "plan_id": &plan_id,
        "reason": "s1 done",
        "steps": [
            {"step_id": "s1", "title": "Step 1", "step_status": "completed"},
            {"step_id": "s2", "title": "Step 2"},
        ],
    });
    let result = registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &amend_args.to_string(),
            Some(session_id),
            Some("turn-2"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["ok"], true, "amend should succeed: {result}");

    // Verify the step status was persisted
    let get_args = json!({"plan_id": &plan_id});
    let result = registry
        .execute(
            "planframe_get",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &get_args.to_string(),
            Some(session_id),
            Some("turn-3"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let result: serde_json::Value = serde_json::from_str(&result).unwrap();
    let steps = result["plan"]["steps"].as_array().unwrap();
    assert_eq!(steps[0]["status"], "completed");
    assert_eq!(steps[1]["status"], "pending");
}

// ---------------------------------------------------------------------------
// Layer 3: unsatisfied_dependencies logic
// ---------------------------------------------------------------------------

fn mk_plan(steps: &[((&str, &[&str]), StepStatus)]) -> PlanFrame {
    PlanFrame {
        plan_id: "p1".into(),
        version: 1,
        parent_version: None,
        workflow_id: "wf".into(),
        root_session_id: "r".into(),
        title: "T".into(),
        objective: "O".into(),
        status: PlanStatus::Approved,
        steps: steps
            .iter()
            .map(|((id, deps), status)| PlanStep {
                step_id: (*id).into(),
                title: format!("step {id}"),
                owner: StepOwner::Agent,
                depends_on: deps.iter().map(|s| s.to_string()).collect(),
                agent_id: None,
                notes: None,
                status: *status,
            })
            .collect(),
        validation_policy: Default::default(),
        capability_envelope: vec![],
        approved_by: None,
        approved_at: None,
        created_by_agent_id: "planner".into(),
        reason: None,
        created_at: "now".into(),
    }
}

#[test]
fn unsatisfied_deps_empty_when_all_completed() {
    use autonoetic_types::plan_frame::unsatisfied_dependencies;
    let plan = mk_plan(&[
        (("s1", &[]), StepStatus::Completed),
        (("s2", &["s1"]), StepStatus::Pending),
    ]);
    assert!(unsatisfied_dependencies(&plan, "s2").is_empty());
}

#[test]
fn unsatisfied_deps_reports_pending_dependency() {
    use autonoetic_types::plan_frame::unsatisfied_dependencies;
    let plan = mk_plan(&[
        (("s1", &[]), StepStatus::Pending),
        (("s2", &["s1"]), StepStatus::Pending),
    ]);
    let unsatisfied = unsatisfied_dependencies(&plan, "s2");
    assert_eq!(unsatisfied.len(), 1);
    assert_eq!(unsatisfied[0].0, "s1");
    assert_eq!(unsatisfied[0].1, StepStatus::Pending);
}

#[test]
fn unsatisfied_deps_reports_in_progress_and_failed() {
    use autonoetic_types::plan_frame::unsatisfied_dependencies;
    let plan = mk_plan(&[
        (("s1", &[]), StepStatus::InProgress),
        (("s2", &[]), StepStatus::Failed),
        (("s3", &["s1", "s2"]), StepStatus::Pending),
    ]);
    let unsatisfied = unsatisfied_dependencies(&plan, "s3");
    assert_eq!(unsatisfied.len(), 2);
    let ids: Vec<&str> = unsatisfied.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.contains(&"s1"));
    assert!(ids.contains(&"s2"));
}

#[test]
fn unsatisfied_deps_skipped_counts_as_unsatisfied() {
    use autonoetic_types::plan_frame::unsatisfied_dependencies;
    let plan = mk_plan(&[
        (("s1", &[]), StepStatus::Skipped),
        (("s2", &["s1"]), StepStatus::Pending),
    ]);
    let unsatisfied = unsatisfied_dependencies(&plan, "s2");
    assert_eq!(unsatisfied.len(), 1);
    assert_eq!(unsatisfied[0].1, StepStatus::Skipped);
}

// ---------------------------------------------------------------------------
// Copilot review fixes: duplicate step_id, unknown step_status, DAG with dups
// ---------------------------------------------------------------------------

#[test]
fn validate_dag_rejects_duplicate_step_id() {
    let steps = vec![
        mk_step("s1", &[]),
        mk_step("s2", &["s1"]),
        mk_step("s1", &["s2"]), // duplicate s1
    ];
    let err = validate_step_dag(&steps).unwrap_err();
    assert!(err.contains("duplicate"), "got: {err}");
}

#[test]
fn unsatisfied_deps_returns_empty_for_unknown_step() {
    // unsatisfied_dependencies returns empty for unknown step_id — the
    // router-level enforcement separately checks step existence and rejects.
    use autonoetic_types::plan_frame::unsatisfied_dependencies;
    let plan = mk_plan(&[
        (("s1", &[]), StepStatus::Pending),
    ]);
    let unsatisfied = unsatisfied_dependencies(&plan, "nonexistent_step");
    assert!(unsatisfied.is_empty(), "unknown step returns no unsatisfied deps");
}
