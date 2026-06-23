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
                // `*` grants participation; `planframe.approve` is an authority
                // that must be granted EXACTLY (a wildcard no longer confers it),
                // so this manifest represents an authorized approver.
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

    // Session Room P1: the approval lands on the canonical timeline as a
    // `plan.approved` event with the plan_id ref, authored by the Operator seat.
    let tl = store
        .list_session_timeline("root-session-003", None, 100, None, None)
        .unwrap();
    let approved = tl
        .entries
        .iter()
        .find(|e| e.event_type == "plan.approved")
        .expect("plan.approved event on the canonical timeline");
    assert_eq!(approved.refs.plan_id.as_deref(), Some(plan_id.as_str()));
    assert_eq!(
        approved.role,
        autonoetic_types::session_timeline::SessionRole::Operator
    );
}

#[test]
fn operator_approve_plan_frame_via_scheduler_ops() {
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

    let session_id = "root-session-operator/planner-op";

    let result = registry
        .execute(
            "planframe_propose",
            &manifest,
            &policy,
            dir.path(),
            Some(&gateway_dir),
            &serde_json::to_string(&json!({
                "title": "Operator Approval Test",
                "objective": "Test chat/CLI operator path"
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

    let pending = autonoetic_gateway::scheduler::pending_plan_frames_for_root(
        store.as_ref(),
        "root-session-operator",
    )
    .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan_id, plan_id);

    let approved = autonoetic_gateway::scheduler::approval::approve_request(
        &config,
        Some(store.as_ref()),
        &format!("apr-plan-{plan_id}-v1"),
        "chat-tui",
        None,
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(approved.status, autonoetic_types::background::ApprovalStatus::Approved);

    let plan = store.load_plan_frame(&plan_id).unwrap().unwrap();
    assert_eq!(plan.status, PlanStatus::Approved);

    assert!(
        autonoetic_gateway::scheduler::pending_plan_frames_for_root(
            store.as_ref(),
            "root-session-operator"
        )
        .unwrap()
        .is_empty()
    );
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

/// Propose → approve → amend with a cosmetic-only change (objective rewording
/// + progress reason). The amendment must INHERIT the prior approval: status
/// stays `approved`, `inherited == true`, and no `plan.pending` checkpoint is
/// emitted (a `plan.approved` with `inherited: true` is emitted instead).
#[test]
fn planframe_amend_inherits_approval_on_cosmetic_change() {
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
    let session_id = "root-session-inherit/planner";

    let propose = json!({
        "title": "Weather Agent",
        "objective": "Build a weather agent",
        "steps": [{ "step_id": "s1", "title": "Implement" }]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    // Approve v1.
    registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();

    // Cosmetic amend: objective rewording + progress reason, same step set.
    let amend = json!({
        "plan_id": plan_id,
        "objective": "Build a reusable weather agent (refined)",
        "reason": "Steps s1 completed; clarifying objective for the record."
    });
    let result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&amend).unwrap(),
            Some(session_id), Some("t3"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["version"], 2, "new revision created");
    assert_eq!(parsed["status"], "approved", "cosmetic amend inherits approval");
    assert_eq!(parsed["inherited"], true);
    assert_eq!(parsed["diff_summary"], "no envelope change");
    assert_eq!(parsed["requires_regate"], false);

    let latest = store.load_plan_frame(&plan_id).unwrap().unwrap();
    assert_eq!(latest.status, PlanStatus::Approved);
    assert_eq!(latest.parent_version, Some(1));
    assert!(latest.approved_at.is_some(), "inherited approval re-stamped");

    // The timeline must NOT carry a plan.pending for the inherited amend; it
    // carries plan.approved with inherited:true instead.
    let tl = store.list_session_timeline("root-session-inherit", None, 50, None, None).unwrap();
    let v2_events: Vec<_> = tl.entries.iter().filter(|e| {
        let p = e.payload.as_deref().unwrap_or("");
        p.contains("\"version\":2")
    }).collect();
    assert!(!v2_events.is_empty(), "v2 should be on the timeline");
    assert!(
        v2_events.iter().all(|e| e.event_type != "plan.pending"),
        "inherited amend must not emit plan.pending"
    );
    assert!(
        v2_events.iter().any(|e| e.event_type == "plan.approved"),
        "inherited amend should emit plan.approved"
    );
}

/// Propose → approve → amend that ADDS a step (envelope expansion). The
/// amendment must RE-OPEN the gate: status `awaiting_approval`, `inherited
/// == false`, `requires_regate == true`, and the diff_summary calls out the
/// added step so the operator sees what they are approving.
#[test]
fn planframe_amend_regates_on_envelope_expansion() {
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
    let session_id = "root-session-regate/planner";

    let propose = json!({
        "title": "Weather Agent",
        "objective": "Build a weather agent",
        "steps": [{ "step_id": "s1", "title": "Implement" }]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();

    // Envelope-expanding amend: add step s2.
    let amend = json!({
        "plan_id": plan_id,
        "steps": [
            { "step_id": "s1", "title": "Implement" },
            { "step_id": "s2", "title": "Package" }
        ],
        "reason": "Added packaging step"
    });
    let result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&amend).unwrap(),
            Some(session_id), Some("t3"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], "awaiting_approval", "envelope change re-gates");
    assert_eq!(parsed["inherited"], false);
    assert_eq!(parsed["requires_regate"], true);
    assert!(
        parsed["diff_summary"].as_str().unwrap().contains("+step s2"),
        "diff_summary should call out the added step: {}",
        parsed["diff_summary"]
    );

    let latest = store.load_plan_frame(&plan_id).unwrap().unwrap();
    assert_eq!(latest.status, PlanStatus::AwaitingApproval);
    assert!(latest.approved_at.is_none(), "re-gated revision is not approved");

    // The timeline must carry plan.pending for the re-gated amend.
    let tl = store.list_session_timeline("root-session-regate", None, 50, None, None).unwrap();
    assert!(
        tl.entries.iter().any(|e| e.event_type == "plan.pending"
            && e.payload.as_deref().unwrap_or("").contains("\"version\":2")),
        "envelope-expanding amend should emit plan.pending for v2"
    );
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

/// Pillar C wiring: approving a plan must surface `grants_materialized` in
/// the response (and the workflow event payload), and an envelope-expanding
/// amend must surface `grants_revoked`. With the real installed agents
/// (planner.default has no NetworkAccess) both counts are 0, but the fields
/// and code paths are exercised end-to-end. The store-level revoke round-trip
/// is tested separately in `approval_grant_revocation_integration`.
#[test]
fn plan_approval_reports_grants_materialized_and_amend_reports_revoke() {
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
    let session_id = "root-grant-wiring/planner";

    // Propose + approve a minimal plan.
    let propose = json!({
        "title": "Grant Wiring",
        "objective": "verify the materialize + revoke fields are wired",
        "steps": [{ "step_id": "s1", "title": "Step 1" }]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    let approve_result = registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&approve_result).unwrap();
    assert_eq!(parsed["status"], "approved");
    // planner.default declares no NetworkAccess → no hosts materialized.
    // The field must exist and be 0 (proves the materialization path ran
    // and correctly found no concrete hosts).
    assert!(parsed["grants_materialized"].is_u64());
    assert_eq!(parsed["grants_materialized"].as_u64().unwrap(), 0);

    // Cosmetic amend on the approved v1 → inherit (no envelope change),
    // status stays approved, grants_revoked stays 0. The cosmetic branch
    // MUST run first so the parent is Approved; an amend on an
    // AwaitingApproval plan never inherits regardless of diff.
    let cosmetic = json!({
        "plan_id": plan_id,
        "objective": "refined wording (no envelope change)",
        "reason": "cosmetic only"
    });
    let cosmetic_result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&cosmetic).unwrap(),
            Some(session_id), Some("t3"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed_cos: serde_json::Value = serde_json::from_str(&cosmetic_result).unwrap();
    assert_eq!(parsed_cos["inherited"], true);
    assert!(parsed_cos["grants_revoked"].is_u64());
    assert_eq!(parsed_cos["grants_revoked"].as_u64().unwrap(), 0);

    // Envelope-expanding amend (add a step) on the inherited-approved v2 →
    // re-gate branch revokes the plan's prior grants by source. With 0 prior
    // grants (planner has no NetworkAccess), 0 are revoked; the field must
    // exist and the code path must execute.
    let amend = json!({
        "plan_id": plan_id,
        "steps": [
            { "step_id": "s1", "title": "Step 1" },
            { "step_id": "s2", "title": "Step 2 (new)" }
        ],
        "reason": "envelope expansion to exercise the revoke branch"
    });
    let amend_result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&amend).unwrap(),
            Some(session_id), Some("t4"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed_amend: serde_json::Value = serde_json::from_str(&amend_result).unwrap();
    assert_eq!(parsed_amend["requires_regate"], true);
    assert_eq!(parsed_amend["inherited"], false);
    assert!(parsed_amend["grants_revoked"].is_u64());
    assert_eq!(parsed_amend["grants_revoked"].as_u64().unwrap(), 0);
}

fn curl_trace(session_id: &str, command: &str) -> autonoetic_types::causal_chain::ExecutionTraceRecord {
    autonoetic_types::causal_chain::ExecutionTraceRecord {
        trace_id: format!("trace-{}", uuid::Uuid::new_v4()),
        event_id: None,
        agent_id: "researcher.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: "2026-06-14T12:00:00Z".to_string(),
        tool_name: "sandbox_exec".to_string(),
        command: Some(command.to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 10,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: Some(format!(r#"{{"command":"{command}"}}"#)),
        result: None,
    }
}

#[test]
fn plan_approval_materializes_declared_capability_envelope() {
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
    let root = "root-cap-envelope";
    let session_id = format!("{root}/planner");

    let propose = json!({
        "title": "Declared envelope",
        "objective": "verify capability_envelope grants",
        "steps": [{ "step_id": "s1", "title": "Step 1" }],
        "capability_envelope": [
            { "type": "NetworkAccess", "hosts": ["api.example.com"] }
        ]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(&session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    let approve_result = registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(&session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&approve_result).unwrap();
    assert_eq!(parsed["grants_materialized"].as_u64().unwrap(), 1);
    assert!(store.session_grants_cover_targets(root, &["api.example.com".to_string()]));

    let proposed = store.get_proposed_envelopes(root).unwrap();
    assert_eq!(proposed.len(), 1);
    assert!(matches!(
        &proposed[0].capability,
        Capability::NetworkAccess { hosts } if hosts == &vec!["api.example.com".to_string()]
    ));
}

#[test]
fn plan_approval_proposes_discovered_hosts_when_envelope_empty() {
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
    let root = "root-discover-envelope";
    let session_id = format!("{root}/planner");

    store
        .create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))
        .unwrap();

    let propose = json!({
        "title": "Discovery envelope",
        "objective": "verify discovery fallback",
        "steps": [{ "step_id": "s1", "title": "Step 1" }]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(&session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    let approve_result = registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(&session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&approve_result).unwrap();
    assert_eq!(parsed["grants_materialized"].as_u64().unwrap(), 0);

    // With auto-lock, the discovered envelope is immediately locked and
    // grants are materialized — no pending proposal remains.
    let proposed = store.get_proposed_envelopes(root).unwrap();
    assert_eq!(proposed.len(), 0);
    let active = store.get_active_envelopes(root).unwrap();
    assert_eq!(active.len(), 1);
    assert!(matches!(
        &active[0].capability,
        Capability::NetworkAccess { hosts } if hosts == &vec!["api.open-meteo.com".to_string()]
    ));
}

#[test]
fn planframe_amend_regates_on_capability_envelope_broadening() {
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
    let session_id = "root-cap-regate/planner";

    let propose = json!({
        "title": "Cap regate",
        "objective": "test capability delta regate",
        "steps": [{ "step_id": "s1", "title": "Step 1" }],
        "capability_envelope": [
            { "type": "NetworkAccess", "hosts": ["api.example.com"] }
        ]
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();

    let amend = json!({
        "plan_id": plan_id,
        "capability_envelope": [
            { "type": "NetworkAccess", "hosts": ["api.example.com", "cdn.example.com"] }
        ],
        "reason": "add CDN host discovered during research"
    });
    let amend_result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&amend).unwrap(),
            Some(session_id), Some("t3"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&amend_result).unwrap();
    assert_eq!(parsed["requires_regate"], true);
    assert_eq!(parsed["inherited"], false);
    assert!(parsed["diff_summary"]
        .as_str()
        .unwrap()
        .contains("+capability"));
}

#[test]
fn planframe_amend_inherits_when_capability_envelope_unchanged() {
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
    let session_id = "root-cap-inherit/planner";

    let envelope = json!([
        { "type": "NetworkAccess", "hosts": ["api.example.com"] }
    ]);
    let propose = json!({
        "title": "Cap inherit",
        "objective": "unchanged capability_envelope should inherit",
        "steps": [{ "step_id": "s1", "title": "Step 1" }],
        "capability_envelope": envelope
    });
    let result = registry
        .execute("planframe_propose", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&propose).unwrap(),
            Some(session_id), Some("t1"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let plan_id = serde_json::from_str::<serde_json::Value>(&result).unwrap()["plan_id"]
        .as_str().unwrap().to_string();

    registry
        .execute("planframe_approve", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&json!({ "plan_id": plan_id })).unwrap(),
            Some(session_id), Some("t2"), Some(&config), Some(store.clone()), None)
        .unwrap();

    let amend = json!({
        "plan_id": plan_id,
        "objective": "progress note only",
        "capability_envelope": envelope,
        "reason": "same network scope"
    });
    let amend_result = registry
        .execute("planframe_amend", &manifest, &policy, dir.path(),
            Some(&gateway_dir), &serde_json::to_string(&amend).unwrap(),
            Some(session_id), Some("t3"), Some(&config), Some(store.clone()), None)
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&amend_result).unwrap();
    assert_eq!(parsed["inherited"], true);
    assert_eq!(parsed["requires_regate"], false);
    assert_eq!(parsed["diff_summary"], "no envelope change");
}
