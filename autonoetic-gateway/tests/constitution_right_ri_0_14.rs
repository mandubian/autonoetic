//! Constitution Ri-0.14: parent wake-up is mechanical and `workflow_wait`
//! remains available as an inspection surface during rollout.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{self, save_task_run, save_workflow_run, ApprovalMetadata};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRun, WorkflowRunStatus};
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
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 4,
            max_spawn_depth: 0,
        }],
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
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

fn seed_task(
    config: &GatewayConfig,
    store: &GatewayStore,
    workflow_id: &str,
    root_session_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let workflow = WorkflowRun {
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: "planner.default".to_string(),
        status: WorkflowRunStatus::WaitingChildren,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        active_task_ids: vec![],
        queued_task_ids: vec![],
        join_policy: Default::default(),
        join_task_ids: vec![task_id.to_string()],
        active_plan_ref: None,
    };
    save_workflow_run(config, Some(store), &workflow)?;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "exec-agent".to_string(),
        session_id: format!("{root_session_id}/exec-agent-001"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Wait for approval and resume mechanically.".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(config, Some(store), &task)
}

#[test]
fn child_waiting_transition_emits_typed_parent_wakeup_event() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-ri014-wakeup";
    let root_session_id = "root-ri014-wakeup";
    let task_id = "task-ri014-wakeup";

    seed_task(&config, store.as_ref(), workflow_id, root_session_id, task_id)?;
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Approval required for sandbox exec".to_string()),
        Some(ApprovalMetadata {
            request_id: "apr-ri014".to_string(),
            kind: "sandbox".to_string(),
            reason: Some("network access".to_string()),
        }),
        None,
    )?;

    let events = store.list_workflow_events(workflow_id)?;
    let waiting = events
        .iter()
        .find(|event| event.event_type == "workflow.child.waiting")
        .expect("workflow.child.waiting event should exist");
    assert_eq!(waiting.payload["child_status"].as_str(), Some("awaiting_approval"));
    assert_eq!(waiting.payload["failure_class"].as_str(), Some("approval_pending"));
    assert_eq!(waiting.payload["retry_advice"].as_str(), Some("wait"));
    assert_eq!(waiting.payload["summary"].as_str(), Some("Approval required for sandbox exec"));
    Ok(())
}

#[test]
fn workflow_wait_remains_available_as_migration_compatibility_surface() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-ri014-wait";
    let root_session_id = "root-ri014-wait";
    let task_id = "task-ri014-wait";

    seed_task(&config, store.as_ref(), workflow_id, root_session_id, task_id)?;
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Approval required for sandbox exec".to_string()),
        Some(ApprovalMetadata {
            request_id: "apr-ri014-wait".to_string(),
            kind: "sandbox".to_string(),
            reason: Some("network access".to_string()),
        }),
        None,
    )?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_id],
        "timeout_secs": 0
    });

    let result = registry.execute(
        "workflow_wait",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-ri014-wait"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));

    let tasks = parsed["tasks"]
        .as_array()
        .expect("workflow.wait should return tasks");
    let task = tasks
        .iter()
        .find(|task| task["task_id"].as_str() == Some(task_id))
        .expect("workflow.wait should include the tracked task");
    assert_eq!(task["status"].as_str(), Some("AwaitingApproval"));
    Ok(())
}
