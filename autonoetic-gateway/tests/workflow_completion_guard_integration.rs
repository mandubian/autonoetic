//! Workflow completion guard integration tests (RFC phase 0).
//!
//! Verifies that once a workflow reaches a terminal state:
//! - `agent_spawn` native tool rejects new tasks.
//! - `workflow.force_complete` rejects mutations.
//! - Pending child-state / join-satisfied notifications are suppressed, not delivered.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_workflow_run, save_task_run, save_workflow_run,
    try_complete_workflow,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus};
use autonoetic_types::notification::{NotificationRecord, NotificationStatus, NotificationType};
use autonoetic_types::workflow::{
    ChildStateNotification, TaskRun, TaskRunStatus, WorkflowRunStatus,
};
use std::sync::Arc;
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
            max_spawn_depth: 0,
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

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir)?;
    let config = GatewayConfig {
        agents_dir,
        default_workflow_wait_secs: 10,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

fn seed_terminal_workflow(
    config: &GatewayConfig,
    store: &GatewayStore,
    root_session_id: &str,
    terminal_status: WorkflowRunStatus,
) -> anyhow::Result<String> {
    let run = ensure_workflow_for_root_session(
        config,
        Some(store),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    let mut workflow = load_workflow_run(config, Some(store), &workflow_id)?
        .expect("workflow should exist after ensure");
    workflow.status = terminal_status;
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &workflow)?;

    let task = TaskRun {
        task_id: "task-old".to_string(),
        workflow_id: workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{root_session_id}/coder-old"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Succeeded,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("done".to_string()),
        join_group: None,
        message: Some("old task".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(config, Some(store), &task)?;

    // save_task_run may have added the task to active_task_ids; clear it for a terminal state.
    let mut workflow = load_workflow_run(config, Some(store), &workflow_id)?
        .expect("workflow should exist");
    workflow.active_task_ids.clear();
    workflow.queued_task_ids.clear();
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &workflow)?;

    Ok(workflow_id)
}

#[test]
fn agent_spawn_rejected_when_workflow_completed() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-spawn-guard";

    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "agent_id": "coder.default",
        "message": "this should fail",
        "async": true
    });

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-spawn-guard"),
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_err(), "agent_spawn should fail for terminal workflow");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("terminal"),
        "error should mention terminal workflow: {}",
        err
    );
    Ok(())
}

#[test]
fn workflow_force_complete_rejected_when_workflow_completed() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-force-guard";

    let workflow_id = seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Completed,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_id": "task-old",
        "status": "succeeded"
    });

    let result = registry.execute(
        "workflow_force_complete",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-force-guard"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(false), "force_complete should fail");
    assert_eq!(
        parsed["error"].as_str(),
        Some("workflow_already_completed"),
        "unexpected error response: {}",
        result
    );
    Ok(())
}

#[test]
fn try_complete_workflow_suppresses_pending_notifications() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-complete-suppress";

    let workflow_id = seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;

    let signal = autonoetic_gateway::scheduler::signal::Signal::ChildStateNotification {
        message: "child done".to_string(),
        notification: ChildStateNotification {
            workflow_id: workflow_id.clone(),
            task_id: "task-old".to_string(),
            child_session_id: format!("{root_session_id}/coder-old"),
            child_status: "succeeded".to_string(),
            failure_class: None,
            install_conflict_detail: None,
            retry_advice: None,
            side_effect_state: None,
            summary: Some("done".to_string()),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let mut n = NotificationRecord::new(
        "ntf-complete-suppress".to_string(),
        NotificationType::ChildStateNotification,
        root_session_id.to_string(),
        serde_json::to_value(&signal)?,
    );
    n.workflow_id = Some(workflow_id.clone());
    store.create_notification_record(&n)?;

    assert_eq!(
        store.list_notifications_for_session(root_session_id, NotificationStatus::Pending)?.len(),
        1
    );

    let completed = try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(completed, "workflow should have transitioned to Completed");

    let pending = store.list_notifications_for_session(root_session_id, NotificationStatus::Pending)?;
    assert!(pending.is_empty(), "pending notifications should be suppressed");

    let suppressed = store.list_notifications_for_session(
        root_session_id,
        NotificationStatus::Suppressed,
    )?;
    assert_eq!(suppressed.len(), 1, "notification should be marked Suppressed");
    Ok(())
}

#[test]
fn try_complete_workflow_blocked_by_pending_escalation() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let root_session_id = "root-escalation-block";

    // Seed a workflow in Resumable with a completed task and join satisfied.
    seed_terminal_workflow(
        &config,
        &store,
        root_session_id,
        WorkflowRunStatus::Resumable,
    )?;

    // Create a pending escalation for this root session.
    let escalation = EscalationMessage::new(
        "esc_test_0001".to_string(),
        "ar.test".to_string(),
        "coder.default".to_string(),
        "rev-001".to_string(),
        vec![],
        "Federation verdicts ready for review".to_string(),
        root_session_id.to_string(),
    );
    store.create_escalation(&escalation)?;

    // Workflow should NOT complete while escalation is pending.
    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(
        !completed,
        "workflow should not complete while escalation is pending"
    );

    // Verify workflow is still Resumable.
    let wf_id =
        autonoetic_gateway::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            &config,
            root_session_id,
        )?
        .expect("workflow should exist");
    let wf = load_workflow_run(&config, Some(store.as_ref()), &wf_id)?
        .expect("workflow run should exist");
    assert_eq!(
        wf.status,
        WorkflowRunStatus::Resumable,
        "workflow should still be Resumable"
    );

    // Now resolve the escalation.
    store.resolve_escalation(
        "esc_test_0001",
        EscalationStatus::Approved,
        "operator",
        Some("approved"),
    )?;

    // Workflow should now complete.
    let completed =
        try_complete_workflow(&config, Some(store.as_ref()), root_session_id)?;
    assert!(
        completed,
        "workflow should complete after escalation is resolved"
    );

    Ok(())
}
