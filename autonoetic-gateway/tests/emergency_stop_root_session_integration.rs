//! Phase 2C: `root_session.emergency_stop` durable halt + checkpoint.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, list_task_runs_for_workflow, load_workflow_run,
    save_task_run, save_workflow_run,
};
use autonoetic_types::background::{UserInteraction, UserInteractionKind, UserInteractionStatus};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use chrono::Utc;
use std::sync::Arc;
use support::TestWorkspace;

fn write_planner_agent(agents_dir: &std::path::Path) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "planner.default"
  name: "planner"
  description: "test"
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Test
"#,
    )?;
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn emergency_stop_aborts_tasks_cancels_interaction_and_checkpoint() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    write_planner_agent(&workspace.agents_dir)?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let root_session = "root-2c-emstop";
    let mut wf = ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_session,
        Some("planner.default"),
    )?;
    wf.status = WorkflowRunStatus::WaitingChildren;
    wf.updated_at = Utc::now().to_rfc3339();
    save_workflow_run(&config, Some(store.as_ref()), &wf)?;

    let ts = Utc::now().to_rfc3339();
    for (tid, st) in [
        ("task-a", TaskRunStatus::Running),
        ("task-b", TaskRunStatus::Pending),
    ] {
        let task = TaskRun {
            task_id: tid.to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: format!("{root_session}/child-{tid}"),
            parent_session_id: root_session.to_string(),
            status: st,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
        };
        save_task_run(&config, Some(store.as_ref()), &task)?;
    }

    store.create_user_interaction(&UserInteraction {
        interaction_id: "ui-2c-1".to_string(),
        session_id: root_session.to_string(),
        root_session_id: root_session.to_string(),
        agent_id: "planner.default".to_string(),
        turn_id: "turn-2c".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "q".to_string(),
        context: None,
        options: vec![],
        allow_freeform: true,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: ts.clone(),
        answered_at: None,
        expires_at: None,
        workflow_id: Some(wf.workflow_id.clone()),
        task_id: None,
        checkpoint_turn_id: None,
    })?;

    let out = execution
        .emergency_stop_root_session(
            root_session,
            "integration test",
            "user",
            "tester",
            "manual",
            None,
        )
        .await?;

    assert_eq!(out["ok"], true);
    let stop_id = out["stop_id"].as_str().expect("stop_id");
    assert!(stop_id.starts_with("estop-"));

    let run = load_workflow_run(&config, Some(store.as_ref()), &wf.workflow_id)?
        .expect("workflow");
    assert_eq!(run.status, WorkflowRunStatus::EmergencyStopped);

    let tasks = list_task_runs_for_workflow(&config, Some(store.as_ref()), &wf.workflow_id)?;
    assert_eq!(tasks.len(), 2);
    for t in tasks {
        assert_eq!(t.status, TaskRunStatus::Aborted);
        let summary = t.result_summary.as_deref().expect("result summary");
        assert!(
            summary.contains(stop_id),
            "summary should cite stop_id: {}",
            summary
        );
    }

    let ui = store
        .get_user_interaction("ui-2c-1")?
        .expect("user interaction");
    assert_eq!(ui.status, UserInteractionStatus::Cancelled);

    let cp = load_latest_checkpoint(&config, root_session)?.expect("checkpoint");
    match &cp.yield_reason {
        YieldReason::EmergencyStop { stop_id: sid } => assert_eq!(sid, stop_id),
        other => panic!("expected EmergencyStop, got {:?}", other),
    }

    let stops = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].status, "stopped");
    assert_eq!(stops[0].stop_id, stop_id);

    Ok(())
}
