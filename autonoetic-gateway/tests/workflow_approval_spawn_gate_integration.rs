use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
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
            id: "planner.default".to_string(),
            name: "planner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 4,
            max_spawn_depth: 2,
        }],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

#[test]
fn agent_spawn_blocked_while_workflow_task_awaiting_approval() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let planner_dir = agents_dir.join("planner.default");
    let child_dir = agents_dir.join("coder.default");
    std::fs::create_dir_all(&planner_dir)?;
    std::fs::create_dir_all(&child_dir)?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let root_session_id = "session-spawn-gate";
    let task_id = "task-awaiting-approval";

    let mut workflow = workflow_store::ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = workflow.workflow_id.clone();
    workflow.status = WorkflowRunStatus::BlockedApproval;
    workflow.join_task_ids = vec![task_id.to_string()];
    workflow.active_task_ids = vec![task_id.to_string()];
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "unit_test_runner.default".to_string(),
        session_id: format!("{root_session_id}/unit_test_runner.default-abc"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::AwaitingApproval,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("awaiting approval apr-gate01".to_string()),
        join_group: None,
        message: Some("Run unit tests".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let mut approval = ApprovalRequest {
        request_id: "apr-gate01".to_string(),
        agent_id: "unit_test_runner.default".to_string(),
        session_id: task.session_id.clone(),
        action: ScheduledAction::SandboxExec {
            command: "python3 /tmp/test_main.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("unit test exec".to_string()),
        evidence_ref: None,
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: Some(workflow_id.to_string()),
        task_id: Some(task_id.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    };
    store.create_approval(&mut approval)?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();

    let spawn_args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "Build another federation batch",
        "async": true
    });
    let err = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &planner_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&spawn_args)?,
            Some(root_session_id),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect_err("spawn should be blocked while approval gate is active");
    let msg = err.to_string();
    assert!(msg.contains("awaiting operator approval"), "{msg}");
    assert!(msg.contains(task_id), "{msg}");
    assert!(msg.contains("apr-gate01"), "{msg}");

    Ok(())
}

#[test]
fn cancelling_awaiting_approval_task_withdraws_pending_approval() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(agents_dir.join("planner.default"))?;

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let task_id = "task-cancel-me";
    let root_session_id = "session-cancel-approval";

    let mut workflow = workflow_store::ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = workflow.workflow_id.clone();
    workflow.status = WorkflowRunStatus::BlockedApproval;
    workflow.join_task_ids = vec![task_id.to_string()];
    workflow.active_task_ids = vec![task_id.to_string()];
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let child_session = format!("{root_session_id}/unit_test_runner.default-cancel");
    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "unit_test_runner.default".to_string(),
        session_id: child_session.clone(),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::AwaitingApproval,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("awaiting approval".to_string()),
        join_group: None,
        message: Some("Run tests".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let mut approval = ApprovalRequest {
        request_id: "apr-cancel-me".to_string(),
        agent_id: "unit_test_runner.default".to_string(),
        session_id: child_session,
        action: ScheduledAction::SandboxExec {
            command: "python3 /tmp/test_main.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: Some(workflow_id.to_string()),
        task_id: Some(task_id.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    };
    store.create_approval(&mut approval)?;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        &workflow_id,
        task_id,
        TaskRunStatus::Cancelled,
        Some("Cancelled after approval timeout".to_string()),
        None,
        None,
    )?;

    let decided = store.get_approval("apr-cancel-me")?.expect("approval row");
    assert_eq!(
        decided.status.map(|s| s.as_str().to_string()),
        Some("cancelled".to_string())
    );

    let wf = workflow_store::load_workflow_run(&config, Some(store.as_ref()), &workflow_id)?
        .expect("workflow");
    assert_ne!(wf.status, WorkflowRunStatus::BlockedApproval);

    Ok(())
}
