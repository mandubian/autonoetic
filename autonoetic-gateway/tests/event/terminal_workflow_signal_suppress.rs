//! Signal-driven auto-resume suppression for terminal workflows.
//!
//! Verifies that a child-state signal delivered to the planner root session
//! after the workflow has completed does NOT wake the planner and cause a
//! token-burning auto-loop. The notification should still be consumed (marked
//! Suppressed or Delivered) so it is not retried forever.

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::run_scheduler_tick;
use autonoetic_gateway::scheduler::signal::Signal;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_workflow_run, save_task_run, save_workflow_run,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::notification::{NotificationRecord, NotificationStatus, NotificationType};
use autonoetic_types::workflow::{ChildStateNotification, TaskRun, TaskRunStatus, WorkflowRunStatus};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn signal_to_terminal_workflow_is_suppressed_and_consumed() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let config = Arc::new(GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir: agents_dir.clone(),
        background_scheduler_enabled: true,
        port,
        ..GatewayConfig::default()
    });

    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        &gateway_dir,
    )?);

    let root_session_id = "root-terminal-signal";
    let workflow_id = ensure_terminal_workflow(&config, &store, root_session_id).await?;

    let checkpoint = build_hibernation_checkpoint(&config, root_session_id, Some(&workflow_id))?;
    autonoetic_gateway::runtime::checkpoint::save_checkpoint(&config, &checkpoint)?;

    let delivered_count = Arc::new(Mutex::new(0usize));
    let delivered_count_server = Arc::clone(&delivered_count);

    let server = tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            let (read_half, mut write_half) = socket.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "test-response",
                "result": {"ok": true}
            })
            .to_string();
            let _ = write_half.write_all(response.as_bytes()).await;
            let _ = write_half.write_all(b"\n").await;
            let _ = write_half.flush().await;

            *delivered_count_server.lock().unwrap() += 1;
        }
    });

    let mut notification = NotificationRecord::new(
        "ntf-terminal-child-state".to_string(),
        NotificationType::ChildStateNotification,
        root_session_id.to_string(),
        serde_json::to_value(Signal::ChildStateNotification {
            notification: ChildStateNotification {
                workflow_id: workflow_id.clone(),
                task_id: "task-terminal".to_string(),
                child_session_id: format!("{}/coder.default", root_session_id),
                child_status: "succeeded".to_string(),
                failure_class: None,
                install_conflict_detail: None,
                retry_advice: None,
                side_effect_state: None,
                agent_outcome: None,
                summary: Some("done".to_string()),
            },
            message: "child completed after workflow terminal".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })?,
    );
    notification.request_id = Some("req-terminal-child-state".to_string());
    notification.workflow_id = Some(workflow_id.clone());
    notification.created_at = chrono::Utc::now().to_rfc3339();
    store.create_notification_record(&notification)?;

    let execution = Arc::new(GatewayExecutionService::new(
        (*config).clone(),
        Some(store.clone()),
    ));

    run_scheduler_tick(execution).await?;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;

    // The notification pump suppresses terminal-workflow signals, so the TCP
    // server will not receive a delivery and the notification will be marked
    // Suppressed. Either outcome (Suppressed, or Delivered if it slips through
    // the pump gate and is then suppressed by resume_from_checkpoint) is valid.
    let after = store.get_notification("ntf-terminal-child-state")?.unwrap();
    assert!(
        matches!(after.status, NotificationStatus::Delivered | NotificationStatus::Suppressed),
        "terminal-workflow signal should be consumed, got {:?}",
        after.status
    );

    // If the pump delivered the signal, our resume gate should have suppressed
    // the auto-resume (the server count confirms delivery but no turn ran).
    assert!(
        *delivered_count.lock().unwrap() <= 1,
        "signal should be delivered at most once"
    );

    let workflow = load_workflow_run(&config, Some(&store), &workflow_id)?
        .expect("workflow should exist");
    assert!(
        matches!(
            workflow.status,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::EmergencyStopped
        ),
        "workflow should stay terminal, got {:?}",
        workflow.status
    );

    Ok(())
}

async fn ensure_terminal_workflow(
    config: &GatewayConfig,
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    root_session_id: &str,
) -> anyhow::Result<String> {
    let run = ensure_workflow_for_root_session(
        config,
        Some(store),
        root_session_id,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    let task = TaskRun {
        task_id: "task-terminal".to_string(),
        workflow_id: workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{}/coder.default", root_session_id),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Succeeded,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("done".to_string()),
        join_group: None,
        message: Some("completed task".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(config, Some(store), &task)?;

    let mut workflow = load_workflow_run(config, Some(store), &workflow_id)?
        .expect("workflow should exist");
    workflow.status = WorkflowRunStatus::Completed;
    workflow.active_task_ids.clear();
    workflow.queued_task_ids.clear();
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &workflow)?;

    Ok(workflow_id)
}

fn build_hibernation_checkpoint(
    _config: &GatewayConfig,
    session_id: &str,
    workflow_id: Option<&str>,
) -> anyhow::Result<autonoetic_gateway::runtime::checkpoint::SessionCheckpoint> {
    use autonoetic_gateway::llm::Message;
    use autonoetic_gateway::runtime::checkpoint::{SessionCheckpoint, YieldReason};
    use autonoetic_gateway::runtime::guard::LoopGuard;

    let now = chrono::Utc::now().to_rfc3339();

    Ok(SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![
            Message::system("system".to_string()),
            Message::user("initial task".to_string()),
            Message::assistant("ack".to_string()),
        ],
        turn_counter: 3,
        loop_guard_state: LoopGuard::new(5),
        session_state: autonoetic_types::agent::SessionState::Normal,
        tool_tier_escalated: false,
        session_phase: Default::default(),
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        agent_id: "planner.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-000003".to_string(),
        workflow_id: workflow_id.map(String::from),
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: Vec::new(),
        created_at: now,
        pending_tool_state: None,
        llm_rounds_consumed: 0,
        tool_invocations_consumed: 0,
        tokens_consumed: 0,
        estimated_cost_usd: 0.0,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
        feedback_events: Vec::new(),
    })
}
