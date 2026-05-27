//! Constitution R-5.14: workflow failures must expose gateway-owned typed
//! classification while preserving the legacy summary surface during rollout.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{self, save_task_run, save_workflow_run};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRun, WorkflowRunStatus};
use std::sync::Arc;
use tempfile::tempdir;

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
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
    };
    save_workflow_run(config, Some(store), &workflow)?;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "builder.default".to_string(),
        session_id: format!("{root_session_id}/builder.default-001"),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Perform durable install work.".to_string()),
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
fn install_conflict_dual_emits_legacy_summary_and_typed_fields() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-r514-install-conflict";
    let root_session_id = "root-r514-install-conflict";
    let task_id = "task-r514-install-conflict";
    let summary = "active revision exists for this agent";

    seed_task(&config, store.as_ref(), workflow_id, root_session_id, task_id)?;
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::Failed,
        Some(summary.to_string()),
        None,
        None,
    )?;

    let events = store.list_workflow_events(workflow_id)?;
    let failed = events
        .iter()
        .find(|event| event.event_type == "task.failed")
        .expect("task.failed event should exist");
    assert_eq!(failed.payload["result_summary"].as_str(), Some(summary));
    assert_eq!(failed.payload["failure_class"].as_str(), Some("install_conflict"));
    assert_eq!(failed.payload["retry_advice"].as_str(), Some("do_not_retry"));
    assert_eq!(failed.payload["side_effect_state"].as_str(), Some("none"));

    let child = events
        .iter()
        .find(|event| event.event_type == "workflow.child.resolved")
        .expect("workflow.child.resolved event should exist");
    assert_eq!(child.payload["failure_class"].as_str(), Some("install_conflict"));
    assert_eq!(child.payload["install_conflict_detail"].as_str(), Some(summary));
    assert_eq!(child.payload["summary"].as_str(), Some(summary));
    Ok(())
}