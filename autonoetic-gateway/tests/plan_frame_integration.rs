mod support;

use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration, ScriptInputMode};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::PlanStatus;
use serde_json::json;
use tempfile::tempdir;

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
            description: "Test collaborative planner".to_string(),
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
                patterns: vec!["*".to_string()],
            },
        ],
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn no_plan_frame_manifest() -> AgentManifest {
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
            id: "agent.no_plan".to_string(),
            name: "No Plan Agent".to_string(),
            description: "Agent without plan frame access".to_string(),
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn make_config(dir: &std::path::Path) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.to_path_buf();
    config
}

#[test]
fn planframe_propose_creates_workflow_and_plan() {
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

    let root_session_id = "root-session-001";
    let session_id = format!("{}/planner-001", root_session_id);

    let args = json!({
        "title": "Build RSS Summarizer Agent",
        "objective": "Create a new agent that can fetch and summarize RSS feeds",
        "steps": [
            {
                "step_id": "draft-skill",
                "title": "Draft SKILL.md",
                "owner": "agent",
                "agent_id": "coder.default"
            },
            {
                "step_id": "review",
                "title": "Operator review",
                "owner": "operator"
            },
            {
                "step_id": "security-review",
                "title": "Static security review",
                "owner": "agent",
                "agent_id": "auditor.default"
            },
            {
                "step_id": "package-install",
                "title": "Package and install",
                "owner": "planner"
            }
        ],
        "validation_policy": {
            "entries": [
                {
                    "validation_id": "static_review",
                    "title": "Static security review",
                    "class": "security_review",
                    "requirement": "required"
                },
                {
                    "validation_id": "unit_tests",
                    "title": "Unit tests",
                    "class": "correctness_check",
                    "requirement": "advisory"
                }
            ]
        }
    });

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some(&session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true, "propose should succeed");
    assert!(parsed["plan_id"].as_str().unwrap().starts_with("plan-"));
    assert_eq!(parsed["status"], "awaiting_approval");
    assert_eq!(parsed["version"], 1);

    let plan_id = parsed["plan_id"].as_str().unwrap();

    let plan = store.load_plan_frame(plan_id).unwrap().unwrap();
    assert_eq!(plan.title, "Build RSS Summarizer Agent");
    assert_eq!(plan.steps.len(), 4);
    assert_eq!(plan.status.as_str(), "awaiting_approval");
    assert_eq!(plan.validation_policy.entries.len(), 2);
    assert_eq!(plan.root_session_id, root_session_id);
    assert_eq!(plan.parent_version, None);

    let wf_id = store.resolve_workflow_id(root_session_id).unwrap().unwrap();
    let wf = autonoetic_gateway::scheduler::workflow_store::load_workflow_run(
        &config,
        Some(&store),
        &wf_id,
    )
    .unwrap()
    .unwrap();
    assert!(wf.active_plan_ref.is_some());
    assert_eq!(wf.active_plan_ref.as_ref().unwrap().plan_id, plan_id);
    assert_eq!(wf.active_plan_ref.as_ref().unwrap().version, 1);
}

#[test]
fn planframe_get_returns_proposed_plan() {
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

    let session_id = "root-session-002/planner-002";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Test Plan",
                "objective": "Test objective"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = registry
        .execute(
            "planframe_get",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["plan"]["plan_id"], plan_id);
    assert_eq!(parsed["plan"]["version"], 1);
}

#[test]
fn planframe_approve_transitions_status() {
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

    let session_id = "root-session-003/planner-003";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Approval Test",
                "objective": "Test approval"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = registry
        .execute(
            "planframe_approve",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "approved_by": "operator"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["status"], "approved");

    let plan = store.load_plan_frame(&plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);
    assert_eq!(plan.approved_by.as_deref(), Some("operator"));
}

#[test]
fn planframe_amend_creates_new_revision() {
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

    let session_id = "root-session-004/planner-004";

    let propose_args = json!({
        "title": "Amend Test",
        "objective": "Test amendment",
        "steps": [
            { "step_id": "step-1", "title": "First step" },
            { "step_id": "step-2", "title": "Second step" }
        ]
    });

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&propose_args).unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    registry
        .execute(
            "planframe_approve",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let result = registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "steps": [
                    { "step_id": "step-1", "title": "First step (updated)" },
                    { "step_id": "step-2", "title": "Second step" },
                    { "step_id": "step-3", "title": "Third step (new)" }
                ],
                "reason": "Added third step after review"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-003"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["version"], 2);
    assert_eq!(parsed["status"], "awaiting_approval");
    assert_eq!(parsed["parent_version"], 1);

    let latest = store.load_plan_frame(&plan_id).unwrap().unwrap();
    assert_eq!(latest.version, 2);
    assert_eq!(latest.parent_version, Some(1));
    assert_eq!(latest.steps.len(), 3);
    assert_eq!(latest.steps[0].title, "First step (updated)");
    assert_eq!(latest.status, PlanStatus::AwaitingApproval);

    let v1 = store.load_plan_frame_revision(&plan_id, 1).unwrap().unwrap();
    assert_eq!(v1.version, 1);
    assert_eq!(v1.steps.len(), 2);
    assert_eq!(v1.steps[0].title, "First step");
    assert_eq!(v1.status, PlanStatus::Approved);
}

#[test]
fn planframe_amend_preserves_original_revision() {
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

    let session_id = "root-session-005/planner-005";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Original Title",
                "objective": "Original objective",
                "steps": [
                    { "step_id": "s1", "title": "Step 1" }
                ]
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "title": "Changed Title",
                "objective": "Changed objective",
                "reason": "Scope change"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let v1 = store.load_plan_frame_revision(&plan_id, 1).unwrap().unwrap();
    assert_eq!(v1.title, "Original Title");
    assert_eq!(v1.objective, "Original objective");
    assert_eq!(v1.status.as_str(), "awaiting_approval");

    let v2 = store.load_plan_frame_revision(&plan_id, 2).unwrap().unwrap();
    assert_eq!(v2.title, "Changed Title");
    assert_eq!(v2.objective, "Changed objective");
    assert_eq!(v2.status.as_str(), "awaiting_approval");
    assert_eq!(v2.parent_version, Some(1));
    assert_eq!(v2.reason.as_deref(), Some("Scope change"));
}

#[test]
fn planframe_history_returns_full_revision_chain() {
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

    let session_id = "root-session-006/planner-006";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "History Test",
                "objective": "Test history"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "title": "History Test v2",
                "reason": "Second revision"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "title": "History Test v3",
                "reason": "Third revision"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-003"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let result = registry
        .execute(
            "planframe_history",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id),
            Some("turn-004"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["count"], 3);

    let revisions = parsed["revisions"].as_array().unwrap();
    assert_eq!(revisions[0]["version"], 1);
    assert_eq!(revisions[0]["parent_version"], serde_json::Value::Null);
    assert_eq!(revisions[1]["version"], 2);
    assert_eq!(revisions[1]["parent_version"], 1);
    assert_eq!(revisions[2]["version"], 3);
    assert_eq!(revisions[2]["parent_version"], 2);
}

#[test]
fn planframe_tools_not_available_without_capability() {
    let registry = default_registry();
    let manifest = no_plan_frame_manifest();

    let definitions = registry.available_definitions(&manifest);
    let plan_tool_names: Vec<&str> = definitions
        .iter()
        .filter(|d| d.name.starts_with("planframe_"))
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        plan_tool_names.is_empty(),
        "planframe tools should not be available without PlanFrameAccess, found: {:?}",
        plan_tool_names
    );
}

#[test]
fn planframe_list_returns_latest_revision_per_plan() {
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

    let session_id = "root-session-007/planner-007";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Plan A",
                "objective": "First plan"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_a_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_a_id,
                "title": "Plan A v2",
                "reason": "Updated"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Plan B",
                "objective": "Second plan"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-003"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_b_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    let result = registry
        .execute(
            "planframe_list",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            "{}",
            Some(session_id),
            Some("turn-list"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["count"], 2);

    let plans = parsed["plans"].as_array().unwrap();
    let plan_a_latest = plans.iter().find(|p| p["plan_id"] == plan_a_id).unwrap();
    assert_eq!(plan_a_latest["version"], 2);
    assert_eq!(plan_a_latest["title"], "Plan A v2");

    let plan_b_latest = plans.iter().find(|p| p["plan_id"] == plan_b_id).unwrap();
    assert_eq!(plan_b_latest["version"], 1);
    assert_eq!(plan_b_latest["title"], "Plan B");
}

#[test]
fn planframe_get_with_version_returns_specific_revision() {
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

    let session_id = "root-session-008/planner-008";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Version Test v1",
                "objective": "Test versioning"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-001"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    registry
        .execute(
            "planframe_amend",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "plan_id": plan_id,
                "title": "Version Test v2",
                "reason": "Update"
            }))
            .unwrap(),
            Some(session_id),
            Some("turn-002"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let result_v1 = registry
        .execute(
            "planframe_get",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id, "version": 1 })).unwrap(),
            Some(session_id),
            Some("turn-003"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed_v1: serde_json::Value = serde_json::from_str(&result_v1).unwrap();
    assert_eq!(parsed_v1["plan"]["title"], "Version Test v1");
    assert_eq!(parsed_v1["plan"]["version"], 1);

    let result_v2 = registry
        .execute(
            "planframe_get",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id, "version": 2 })).unwrap(),
            Some(session_id),
            Some("turn-004"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed_v2: serde_json::Value = serde_json::from_str(&result_v2).unwrap();
    assert_eq!(parsed_v2["plan"]["title"], "Version Test v2");
    assert_eq!(parsed_v2["plan"]["version"], 2);

    let result_latest = registry
        .execute(
            "planframe_get",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id),
            Some("turn-005"),
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();

    let parsed_latest: serde_json::Value = serde_json::from_str(&result_latest).unwrap();
    assert_eq!(parsed_latest["plan"]["version"], 2);
}
