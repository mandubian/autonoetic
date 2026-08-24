//! Constitution P-6.25: stage-local retries must be bounded and observable.

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
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

#[test]
fn retry_budget_exhaustion_emits_event_and_stops_further_retry_progress() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-r625-budget";
    let root_session_id = "root-r625-budget";
    let task_id = "task-r625-budget";

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
        reactivated_for_root_spawn: false,
    };
    save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

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
        message: Some("Retry transient infra exactly once.".to_string()),
        metadata: None,
        retry_count: 1,
        last_failure_class: None,
        retry_policy: Some(serde_json::json!({
            "transient_infra": { "max_retries": 1 }
        })),
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(&config, Some(store.as_ref()), &task)?;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::Failed,
        Some("connection refused by upstream".to_string()),
        None,
        None,
    )?;

    let events = store.list_workflow_events(workflow_id)?;
    let exhausted = events
        .iter()
        .find(|event| event.event_type == "workflow.stage_budget_exhausted")
        .expect("workflow.stage_budget_exhausted event should exist");
    assert_eq!(exhausted.payload["failure_class"].as_str(), Some("transient_infra"));
    assert_eq!(exhausted.payload["retry_advice"].as_str(), Some("do_not_retry"));
    assert_eq!(exhausted.payload["retry_count"].as_u64(), Some(1));
    assert_eq!(exhausted.payload["max_retries"].as_u64(), Some(1));

    assert!(
        !events.iter().any(|event| {
            event.event_type == "task.updated" && event.payload["status"].as_str() == Some("runnable")
        }),
        "exhausted retry budget must not requeue the task"
    );

    let persisted = workflow_store::load_task_run(&config, Some(store.as_ref()), workflow_id, task_id)?
        .expect("task should still exist");
    assert_eq!(persisted.status, TaskRunStatus::Failed);
    assert_eq!(persisted.retry_count, 1);
    Ok(())
}