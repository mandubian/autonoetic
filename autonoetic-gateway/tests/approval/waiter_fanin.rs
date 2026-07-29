//! #723: an approval that sibling sessions joined (root-scoped, identical-action
//! dedup) fans in to every waiter on resolution — approve flips them all to
//! Runnable, reject/cancel fails them all — and the waiter rows are cleared.

use autonoetic_gateway::scheduler::approval::{
    approve_request_with_options, reject_request_with_options, ApproveOptions,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_task_run, save_task_run,
};
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
use tempfile::tempdir;

const ROOT: &str = "root-fanin";

fn store() -> (tempfile::TempDir, GatewayStore, autonoetic_types::config::GatewayConfig) {
    let dir = tempdir().unwrap();
    let gw = dir.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    let store = GatewayStore::open(&gw).unwrap();
    let cfg = autonoetic_types::config::GatewayConfig {
        agents_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    (dir, store, cfg)
}

fn awaiting_task(workflow_id: &str, session_id: &str) -> TaskRun {
    let ts = chrono::Utc::now().to_rfc3339();
    TaskRun {
        task_id: format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        workflow_id: workflow_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: session_id.to_string(),
        parent_session_id: ROOT.to_string(),
        status: TaskRunStatus::AwaitingApproval,
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

/// Standalone-risk action (no dwell/confirm) bound to a workflow task, so
/// approve/reject applies without dwell gating getting in the way.
fn bound_approval(request_id: &str, session_id: &str, wf: &str, task_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: session_id.to_string(),
        action: ScheduledAction::WriteFile {
            path: "/tmp/out".to_string(),
            content: "x".to_string(),
            requires_approval: true,
            evidence_ref: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        workflow_id: Some(wf.to_string()),
        task_id: Some(task_id.to_string()),
        root_session_id: Some(ROOT.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

#[test]
fn approve_fans_in_to_joined_waiters() {
    let (_d, store, cfg) = store();
    let wf = ensure_workflow_for_root_session(&cfg, Some(&store), ROOT, None).unwrap();

    // Two sibling tasks, both parked AwaitingApproval.
    let a = awaiting_task(&wf.workflow_id, "root-fanin/a");
    let b = awaiting_task(&wf.workflow_id, "root-fanin/b");
    save_task_run(&cfg, Some(&store), &a).unwrap();
    save_task_run(&cfg, Some(&store), &b).unwrap();

    // Primary approval owned by task A; task B joined it as a waiter (#723).
    let mut req = bound_approval("apr-fanin-1", "root-fanin/a", &wf.workflow_id, &a.task_id);
    store.create_approval(&mut req).unwrap();
    store
        .add_approval_waiter("apr-fanin-1", "root-fanin/b", Some(&wf.workflow_id), Some(&b.task_id))
        .unwrap();

    approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-fanin-1",
        "operator",
        None,
        None,
        Some(&ApprovalLevel::Operator),
        None,
        ApproveOptions::default(),
    )
    .unwrap();

    let a2 = load_task_run(&cfg, Some(&store), &wf.workflow_id, &a.task_id).unwrap().unwrap();
    let b2 = load_task_run(&cfg, Some(&store), &wf.workflow_id, &b.task_id).unwrap().unwrap();
    assert_eq!(a2.status, TaskRunStatus::Runnable, "primary task resumes");
    assert_eq!(b2.status, TaskRunStatus::Runnable, "joined waiter also resumes");
    assert!(
        store.list_approval_waiters("apr-fanin-1").unwrap().is_empty(),
        "waiters cleared after resolution"
    );
}

#[test]
fn reject_fails_joined_waiters() {
    let (_d, store, cfg) = store();
    let wf = ensure_workflow_for_root_session(&cfg, Some(&store), ROOT, None).unwrap();

    let a = awaiting_task(&wf.workflow_id, "root-fanin/a");
    let b = awaiting_task(&wf.workflow_id, "root-fanin/b");
    save_task_run(&cfg, Some(&store), &a).unwrap();
    save_task_run(&cfg, Some(&store), &b).unwrap();

    let mut req = bound_approval("apr-fanin-2", "root-fanin/a", &wf.workflow_id, &a.task_id);
    store.create_approval(&mut req).unwrap();
    store
        .add_approval_waiter("apr-fanin-2", "root-fanin/b", Some(&wf.workflow_id), Some(&b.task_id))
        .unwrap();

    reject_request_with_options(
        &cfg,
        Some(&store),
        "apr-fanin-2",
        "operator",
        Some("not allowed".to_string()),
        None,
        ApproveOptions::default(),
    )
    .unwrap();

    let a2 = load_task_run(&cfg, Some(&store), &wf.workflow_id, &a.task_id).unwrap().unwrap();
    let b2 = load_task_run(&cfg, Some(&store), &wf.workflow_id, &b.task_id).unwrap().unwrap();
    assert_eq!(a2.status, TaskRunStatus::Failed, "primary task fails");
    assert_eq!(b2.status, TaskRunStatus::Failed, "joined waiter also fails");
    assert!(store.list_approval_waiters("apr-fanin-2").unwrap().is_empty());
}
