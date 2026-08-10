//! Constitution R+12 — Orphan-child reaper aborts the live Tokio task handle.
//!
//! Companion to `constitution_lifecycle_orphan_reaper.rs`. Those tests prove the
//! reaper updates the DB (transcript -> failed, task -> Cancelled, causal event).
//! These prove the *runtime* side of graceful child abandonment (#618): after
//! the DB cancel, the still-running Tokio task handle registered in the
//! `ActiveExecutionRegistry` is actually aborted — closing the zombie window
//! where the DB said "Cancelled" but the future kept running. And, critically,
//! a child parked at an approval gate is NOT aborted (the existing exception).


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::reap_orphaned_sessions;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, save_task_run, save_workflow_run,
};
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::causal_chain::SessionTranscriptRecord;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use std::sync::Arc;
use crate::support::TestWorkspace;

fn make_transcript(
    session_id: &str,
    root_session_id: &str,
    agent_id: &str,
    status: &str,
) -> SessionTranscriptRecord {
    let now = chrono::Utc::now().to_rfc3339();
    SessionTranscriptRecord {
        transcript_id: format!("tid-{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
        session_id: session_id.to_string(),
        root_session_id: root_session_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: None,
        user_id: None,
        started_at: now.clone(),
        ended_at: if status != "active" { Some(now) } else { None },
        status: status.to_string(),
        turn_count: 0,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    }
}

fn running_task(
    workflow_id: &str,
    task_id: &str,
    child_session_id: &str,
    parent_session_id: &str,
) -> TaskRun {
    let ts = chrono::Utc::now().to_rfc3339();
    TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: child_session_id.to_string(),
        parent_session_id: parent_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: ts.clone(),
        updated_at: ts,
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: None,
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    }
}

/// An abandoned (orphan) child with an in-flight task whose Tokio handle is
/// registered in `active_executions` → after `reap_orphaned_sessions`, the
/// handle is aborted: the spawned future is cancelled and the registry no
/// longer holds it.
#[tokio::test]
async fn orphan_reaper_aborts_in_flight_handle_for_abandoned_child() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-handle-abort";
    let parent_id = "root-handle-abort/planner.default-aaaa1111";
    let child_id = "root-handle-abort/planner.default-aaaa1111/coder.default-bbbb2222";

    // Root completed, immediate parent terminal, child still active -> orphan.
    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "completed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "planner.default",
            "failed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();

    // Create a workflow + a Running task whose session is the orphan child.
    let mut wf = ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_id,
        Some("planner.default"),
    )
    .unwrap();
    wf.status = WorkflowRunStatus::WaitingChildren;
    save_workflow_run(&config, Some(store.as_ref()), &wf).unwrap();
    let task_id = "task-orphan-running";
    let task = running_task(&wf.workflow_id, task_id, child_id, parent_id);
    save_task_run(&config, Some(store.as_ref()), &task).unwrap();

    let execution = Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    // Spawn a never-ending future and register its abort handle, mimicking a
    // live workflow-task execution.
    let join = tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    execution.active_executions().register_workflow_task(
        &wf.workflow_id,
        task_id,
        join.abort_handle(),
    );

    reap_orphaned_sessions(execution.clone())
        .await
        .expect("reaper should succeed");

    // The reaper must have aborted (and removed) the live handle. Bound the
    // await so a regression (handle not aborted) fails fast instead of hanging
    // CI forever on the never-ending future.
    let join_result = tokio::time::timeout(std::time::Duration::from_secs(5), join)
        .await
        .expect("reaper should have aborted the handle; await timed out (regression)");
    assert!(
        join_result.is_err() && join_result.unwrap_err().is_cancelled(),
        "the in-flight task handle for an abandoned child should be aborted by the reaper"
    );

    // And the handle is gone from the registry: a fresh abort finds nothing.
    let aborted_again = execution
        .active_executions()
        .abort_workflow_tasks(&wf.workflow_id, &[task_id.to_string()]);
    assert_eq!(
        aborted_again, 0,
        "handle should already be removed by the reaper's abort"
    );

    // Sanity: the child is reaped (failed) as before.
    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child transcript");
    assert_eq!(child.status, "failed");
}

/// A child parked at an approval gate must NOT have its handle aborted: the
/// existing approval-parked exception holds for the runtime side too. Its task
/// stays non-terminal and its registered handle is left running.
#[tokio::test]
async fn orphan_reaper_does_not_abort_handle_for_approval_parked_child() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-parked-noabort";
    let parent_id = "root-parked-noabort/agent-factory.default-aaaa1111";
    let child_id =
        "root-parked-noabort/agent-factory.default-aaaa1111/specialized_builder.default-bbbb2222";

    // Root alive, immediate parent between turns (status `completed` from
    // close_session, lifecycle `hibernated` from the yield checkpoint), child
    // still active. A between-turn parent is resumable, so its children are
    // protected from orphaning — the approval-parked exception is exercised on
    // top of that protection.
    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "agent-factory.default",
            "completed",
        ))
        .unwrap();
    // Mirrors the real flow: `close_session` writes status `completed` while
    // `save_yield_checkpoint` writes lifecycle `hibernated`. Without this the
    // upsert derives `terminated:completed`, making the parent terminal and the
    // child orphanable — the opposite of what this test asserts.
    store.set_session_lifecycle_state(parent_id, "hibernated").unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "specialized_builder.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();

    // A Running task for the child.
    let mut wf = ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_id,
        Some("planner.default"),
    )
    .unwrap();
    wf.status = WorkflowRunStatus::WaitingChildren;
    save_workflow_run(&config, Some(store.as_ref()), &wf).unwrap();
    let task_id = "task-parked-running";
    let task = running_task(&wf.workflow_id, task_id, child_id, parent_id);
    save_task_run(&config, Some(store.as_ref()), &task).unwrap();

    // The child is parked at a pending operator approval (e.g. a promotion gate).
    let mut approval = ApprovalRequest {
        request_id: "apr-parked-noabort".to_string(),
        agent_id: "specialized_builder.default".to_string(),
        session_id: child_id.to_string(),
        action: ScheduledAction::SandboxExec {
            command: "install".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        workflow_id: Some(wf.workflow_id.clone()),
        task_id: Some(task_id.to_string()),
        root_session_id: Some(root_id.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,

        expires_at: None,
    };
    store.create_approval(&mut approval).unwrap();

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    // Register a live handle for the parked child's task.
    let join = tokio::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    execution.active_executions().register_workflow_task(
        &wf.workflow_id,
        task_id,
        join.abort_handle(),
    );

    reap_orphaned_sessions(execution.clone())
        .await
        .expect("reaper should succeed");

    // The parked child is skipped: its future is still running (not cancelled).
    assert!(
        !join.is_finished(),
        "an approval-parked child's handle must NOT be aborted by the reaper"
    );

    // And the handle is still registered: a manual abort now finds and aborts it
    // (returns 1), proving the reaper left it in place.
    let aborted = execution
        .active_executions()
        .abort_workflow_tasks(&wf.workflow_id, &[task_id.to_string()]);
    assert_eq!(
        aborted, 1,
        "the parked child's handle should still have been registered after the reaper (reaper must not touch it)"
    );

    // Child transcript stays active (not reaped).
    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child transcript");
    assert_eq!(
        child.status, "active",
        "child parked at an approval gate must NOT be reaped"
    );

    // Clean up the spawned task.
    let _ = join.await;
}
