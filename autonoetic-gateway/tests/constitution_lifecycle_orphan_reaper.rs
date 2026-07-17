//! Constitution R+12 — Orphan-child reaper on parent termination.

mod support;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::reap_orphaned_sessions;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::causal_chain::SessionTranscriptRecord;
use autonoetic_types::config::GatewayConfig;
use support::TestWorkspace;

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

#[tokio::test]
async fn orphan_reaper_cancels_child_when_parent_ends() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-001";
    let parent_id = "root-session-001/planner.default-abcd1234";
    let child_id = "root-session-001/planner.default-abcd1234/coder.default-efgh5678";

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
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child transcript should exist");
    assert_eq!(
        child.status, "failed",
        "orphaned child should be marked as failed"
    );

    let events = store
        .search_causal_events(Some(child_id), None, 100)
        .unwrap();
    let reaped = events.iter().find(|e| e.action == "parent_terminated");
    assert!(
        reaped.is_some(),
        "should emit parent_terminated causal event"
    );
    let ev = reaped.unwrap();
    assert_eq!(ev.category, "session");
    assert_eq!(ev.status, "error");
    assert!(ev.enforced_rules.contains(&"R+12".to_string()));
    assert_eq!(ev.target.as_deref(), Some(parent_id));
}

#[tokio::test]
async fn orphan_reaper_ignores_active_parent() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-002";
    let parent_id = "root-session-002/planner.default-aaaa1111";
    let child_id = "root-session-002/planner.default-aaaa1111/coder.default-bbbb2222";

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
            "planner.default",
            "active",
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
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child should exist");
    assert_eq!(
        child.status, "active",
        "child with active parent should NOT be reaped"
    );
}

#[tokio::test]
async fn orphan_reaper_noop_without_store() {
    let ws = TestWorkspace::new().unwrap();
    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, None));
    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed without store");
}

#[tokio::test]
async fn orphan_reaper_handles_multiple_orphans() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-003";
    let parent_id = "root-session-003/planner.default-cccc3333";
    let child1_id = "root-session-003/planner.default-cccc3333/coder.default-dddd4444";
    let child2_id = "root-session-003/planner.default-cccc3333/researcher.default-eeee5555";

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
            "completed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child1_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child2_id,
            root_id,
            "researcher.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    for cid in &[child1_id, child2_id] {
        let child = store
            .find_transcript_by_session_id(cid)
            .unwrap()
            .expect("child should exist");
        assert_eq!(
            child.status, "failed",
            "orphaned child {} should be marked as failed",
            cid
        );
    }
}

#[tokio::test]
async fn orphan_reaper_does_not_reap_suspended_parent() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-004";
    let parent_id = "root-session-004/planner.default-ffff6666";
    let child_id = "root-session-004/planner.default-ffff6666/coder.default-gggg7777";

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
            "planner.default",
            "suspended",
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
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child should exist");
    assert_eq!(
        child.status, "active",
        "child with suspended parent should NOT be reaped"
    );
}

/// A child parked at an approval gate must NOT be reaped when its immediate
/// parent is hibernated (between turns). In the async-spawn pattern the parent
/// legitimately ends its turn while the child stays suspended awaiting an
/// operator decision. The hibernated parent will resume when the gate resolves.
/// #742: the parent is `hibernated`, not `terminated`; children of hibernated
/// parents are protected by design.
#[tokio::test]
async fn orphan_reaper_skips_child_parked_at_approval() {
    use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};

    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-003";
    let parent_id = "root-session-003/agent-factory.default-aaaa1111";
    let child_id =
        "root-session-003/agent-factory.default-aaaa1111/specialized_builder.default-bbbb2222";

    // Root alive, immediate parent's turn ended (completed), child still active.
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
            "active",
        ))
        .unwrap();
    // #742: parent between turns → hibernated (not terminated).
    store
        .set_session_lifecycle_state(parent_id, "hibernated")
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "specialized_builder.default",
            "active",
        ))
        .unwrap();

    // The child is parked at a pending operator approval (e.g. a promotion gate).
    let mut approval = ApprovalRequest {
        request_id: "apr-parked-001".to_string(),
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
        workflow_id: None,
        task_id: None,
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
    assert!(
        store
            .get_pending_approvals()
            .unwrap()
            .iter()
            .any(|a| a.request_id == "apr-parked-001"),
        "approval should be pending"
    );

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child should exist");
    assert_eq!(
        child.status, "active",
        "child parked at an approval gate must NOT be reaped"
    );

    // And no parent_terminated reap event was emitted for it.
    let events = store
        .search_causal_events(Some(child_id), None, 100)
        .unwrap();
    assert!(
        !events.iter().any(|e| e.action == "parent_terminated"),
        "no reap event should be emitted for an approval-parked child"
    );
}

/// A child bound to a non-terminal workflow task must NOT be reaped when the
/// parent is hibernated (between turns). The workflow system will wake the
/// parent when the task finishes. #742: a hibernated parent is alive, not
/// terminated — children are protected by design.
#[tokio::test]
async fn orphan_reaper_skips_workflow_task_when_parent_between_turns() {
    use autonoetic_gateway::scheduler::workflow_store::{
        ensure_workflow_for_root_session, save_task_run, save_workflow_run,
    };
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};

    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-wf-exempt";
    // Parent IS the root (planner hibernated between turns).
    let parent_id = root_id;
    let child_id = "root-wf-exempt/coder.default-aaaa1111";

    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.collaborative",
            "active",
        ))
        .unwrap();
    // #742: root between turns → hibernated (not terminated).
    store
        .set_session_lifecycle_state(root_id, "hibernated")
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

    // Workflow with a Running task bound to the child session.
    let mut wf = ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_id,
        Some("planner.collaborative"),
    )
    .unwrap();
    wf.status = WorkflowRunStatus::WaitingChildren;
    save_workflow_run(&config, Some(store.as_ref()), &wf).unwrap();
    let task = TaskRun {
        task_id: "task-wf-running".to_string(),
        workflow_id: wf.workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: child_id.to_string(),
        parent_session_id: parent_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.collaborative".to_string()),
        result_summary: None,
        join_group: None,
        message: None,
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(&config, Some(store.as_ref()), &task).unwrap();

    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child transcript");
    assert_eq!(
        child.status, "active",
        "child with active workflow task must NOT be reaped when parent is between turns (completed)"
    );

    let events = store
        .search_causal_events(Some(child_id), None, 100)
        .unwrap();
    assert!(
        !events.iter().any(|e| e.action == "parent_terminated"),
        "no reap event should be emitted for a workflow-active child with parent between turns"
    );
}
