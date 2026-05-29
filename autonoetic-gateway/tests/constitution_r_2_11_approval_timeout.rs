//! Constitution P-2.11 & P-7.11 — approval timeout marks the task Failed
//! while preserving the continuation for operator-driven resume.

mod support;

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::continuation::{
    continuation_path, save_continuation, PendingApprovalToolCall, TurnContinuation,
};
use autonoetic_gateway::runtime::guard::LoopGuardState;
use autonoetic_gateway::runtime::session_report::SessionReportWriter;
use autonoetic_gateway::scheduler::{
    gateway_store::GatewayStore, run_scheduler_tick, workflow_store,
};
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRun, WorkflowRunStatus};
use std::sync::Arc;

fn default_loop_guard_state() -> LoopGuardState {
    LoopGuardState {
        max_loops_without_progress: 5,
        max_tool_failures: 5,
        max_consecutive_same_progress: 1,
        max_child_failures: 3,
        current_loops: 0,
        tool_failure_counts: std::collections::HashMap::new(),
        last_progress_fingerprint: None,
        consecutive_progress_count: 0,
        child_failure_count: 0,
        ..Default::default()
    }
}

#[tokio::test]
async fn r_2_11_timed_out_approval_marks_task_failed_and_preserves_continuation(
) -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.approval_timeout_secs = 1;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-r-2-11";
    let root_session_id = "root-r-2-11";
    let task_id = "task-r-2-11";
    let child_session_id = "root-r-2-11/child-1";

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
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "exec-agent".to_string(),
        session_id: child_session_id.to_string(),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::AwaitingApproval,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("run command".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let continuation = TurnContinuation {
        history: vec![Message::user("run command")],
        assistant_message: Message::assistant("pending approval"),
        completed_tool_results: vec![],
        pending_tool_call: PendingApprovalToolCall {
            call_id: "call-r-2-11".to_string(),
            tool_name: "sandbox_exec".to_string(),
            arguments: r#"{"command":"curl https://example.com"}"#.to_string(),
            approval_response: r#"{"approval_required":true}"#.to_string(),
        },
        remaining_tool_calls: vec![],
        approval_request_id: "apr-r-2-11".to_string(),
        pending_action: None,
        workflow_id: Some(workflow_id.to_string()),
        task_id: Some(task_id.to_string()),
        session_id: child_session_id.to_string(),
        turn_id: "turn-0001".to_string(),
        suspended_at: (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
        loop_guard_state: default_loop_guard_state(),
        session_state: Default::default(),
    };
    save_continuation(&config, task_id, &continuation)?;

    run_scheduler_tick(execution).await?;

    let updated =
        workflow_store::load_task_run(&config, Some(store.as_ref()), workflow_id, task_id)?
            .expect("task should still exist");
    assert_eq!(updated.status, TaskRunStatus::Failed);
    assert_eq!(
        updated.result_summary.as_deref(),
        Some("Approval timed out")
    );

    let cont_path = continuation_path(&config, task_id);
    assert!(
        cont_path.exists(),
        "continuation must be preserved after timeout so operator-approved resume is still possible"
    );

    Ok(())
}

#[tokio::test]
async fn r_7_11_approval_timeout_records_resolution_in_session_report() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.approval_timeout_secs = 1;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-r-7-11";
    let root_session_id = "root-r-7-11";
    let task_id = "task-r-7-11";
    let child_session_id = "root-r-7-11/child-1";
    let agent_id = "exec-agent";

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
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: agent_id.to_string(),
        session_id: child_session_id.to_string(),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::AwaitingApproval,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("run command".to_string()),
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    // Pre-create the approval entry in the session report (simulates the
    // approval_required tool result that was recorded before session suspension).
    {
        let mut report = SessionReportWriter::open(&gateway_dir, child_session_id, agent_id)?;
        report.start_session("Test session")?;
        report.start_turn(Some("turn-1"))?;
        report.record_tool_completed(
            "sandbox_exec",
            r#"{"approval_required":true,"request_id":"apr-r-7-11","approval":{"kind":"sandbox_exec","summary":"remote access","reason":"curl example.com"},"ok":false}"#,
            None,
            Some("turn-1"),
            None,
        )?;
    }

    let continuation = TurnContinuation {
        history: vec![Message::user("run command")],
        assistant_message: Message::assistant("pending approval"),
        completed_tool_results: vec![],
        pending_tool_call: PendingApprovalToolCall {
            call_id: "call-r-7-11".to_string(),
            tool_name: "sandbox_exec".to_string(),
            arguments: r#"{"command":"curl https://example.com"}"#.to_string(),
            approval_response: r#"{"approval_required":true}"#.to_string(),
        },
        remaining_tool_calls: vec![],
        approval_request_id: "apr-r-7-11".to_string(),
        pending_action: None,
        workflow_id: Some(workflow_id.to_string()),
        task_id: Some(task_id.to_string()),
        session_id: child_session_id.to_string(),
        turn_id: "turn-0001".to_string(),
        suspended_at: (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
        loop_guard_state: default_loop_guard_state(),
        session_state: Default::default(),
    };
    save_continuation(&config, task_id, &continuation)?;

    run_scheduler_tick(execution).await?;

    // Verify the session report was updated with the timeout resolution.
    let session_dir = gateway_dir.join("sessions").join(root_session_id);
    let report_json = std::fs::read_to_string(session_dir.join("session_report.live.json"))?;
    assert!(
        report_json.contains("\"timed_out\""),
        "session report must record the timeout decision for the approval, got: {}",
        &report_json[..report_json.len().min(500)]
    );
    assert!(
        report_json.contains("apr-r-7-11"),
        "session report must reference the approval request ID"
    );

    Ok(())
}
