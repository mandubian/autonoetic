//! Phase 2C: `root_session.emergency_stop` durable halt + checkpoint.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::scheduler::gateway_store::{ActiveExecutionRecord, GatewayStore};
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, list_task_runs_for_workflow, load_workflow_run,
    save_task_run, save_workflow_run,
};
use autonoetic_types::background::{
    ApprovalRequest, ScheduledAction, UserInteraction, UserInteractionKind,
    UserInteractionStatus,
};
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

// ---------------------------------------------------------------------------
// 2C.10: Sandbox child process kill
// ---------------------------------------------------------------------------

#[serial_test::serial]
#[tokio::test]
async fn emergency_stop_kills_sandbox_child_process() -> anyhow::Result<()> {
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
    let registry = execution.active_executions();

    let root_session = "root-2c-sandbox-kill";

    // Spawn a long-running child process
    let mut child = std::process::Command::new("sleep")
        .arg("60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let pid = child.id();
    assert!(pid > 0, "child process should have a valid PID");

    // Register the PID in the in-memory registry
    let _guard = registry.register_sandbox_child_pid(root_session, pid);

    // Verify process is alive before kill
    let status_before = child.try_wait()?;
    assert!(status_before.is_none(), "process should still be running");

    // Kill via the registry
    let killed = registry.kill_sandbox_children_for_root(root_session);
    assert_eq!(killed.len(), 1);
    assert_eq!(killed[0], pid);

    // Give the process a moment to terminate
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Verify the process is now dead
    let status_after = child.try_wait()?;
    assert!(
        status_after.is_some(),
        "process should have been killed by SIGKILL"
    );
    // Process should have been killed by signal (exit code varies by platform)
    // On Linux, killed processes have signal 9; try_wait returns ExitStatus

    // Verify killing a second time with no remaining processes returns empty
    let killed_again = registry.kill_sandbox_children_for_root(root_session);
    assert_eq!(killed_again.len(), 0);

    Ok(())
}

// ---------------------------------------------------------------------------
// 2C.11: Emergency stop cancels pending approval + user interaction
// ---------------------------------------------------------------------------

#[serial_test::serial]
#[tokio::test]
async fn emergency_stop_cancels_pending_approval_and_interaction() -> anyhow::Result<()> {
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

    let root_session = "root-2c-approval-cancel";
    let ts = Utc::now().to_rfc3339();

    // Set up a workflow with one running task
    let mut wf = ensure_workflow_for_root_session(
        &config,
        Some(store.as_ref()),
        root_session,
        Some("planner.default"),
    )?;
    wf.status = WorkflowRunStatus::WaitingChildren;
    wf.updated_at = ts.clone();
    save_workflow_run(&config, Some(store.as_ref()), &wf)?;

    let task = TaskRun {
        task_id: "task-running".to_string(),
        workflow_id: wf.workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{root_session}/child-running"),
        parent_session_id: root_session.to_string(),
        status: TaskRunStatus::Running,
        created_at: ts.clone(),
        updated_at: ts.clone(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: None,
        metadata: None,
    };
    save_task_run(&config, Some(store.as_ref()), &task)?;

    // Create a pending approval
    store.create_approval(&ApprovalRequest {
        request_id: "apr-2c-test".to_string(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{root_session}/child-running"),
        root_session_id: Some(root_session.to_string()),
        workflow_id: Some(wf.workflow_id.clone()),
        task_id: Some("task-running".to_string()),
        action: ScheduledAction::SandboxExec {
            command: "rm -rf /".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
        },
        created_at: ts.clone(),
        reason: Some("needs approval for dangerous command".to_string()),
        evidence_ref: None,
        status: None,
        decided_at: None,
        decided_by: None,
    })?;

    // Create a pending user interaction
    store.create_user_interaction(&UserInteraction {
        interaction_id: "ui-2c-approve".to_string(),
        session_id: root_session.to_string(),
        root_session_id: root_session.to_string(),
        agent_id: "planner.default".to_string(),
        turn_id: "turn-2c-approve".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "Which path?".to_string(),
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

    // Verify both are pending before stop
    let approvals_before = store.get_pending_approvals_for_root(root_session)?;
    assert_eq!(approvals_before.len(), 1);
    let ui_before = store
        .get_user_interaction("ui-2c-approve")?
        .expect("user interaction");
    assert_eq!(ui_before.status, UserInteractionStatus::Pending);

    // Execute emergency stop
    let out = execution
        .emergency_stop_root_session(
            root_session,
            "test cancel pending gates",
            "user",
            "tester",
            "manual",
            None,
        )
        .await?;

    assert_eq!(out["ok"], true);
    let stop_id = out["stop_id"].as_str().expect("stop_id");

    // Verify approval is cancelled
    let approvals_after = store.get_pending_approvals_for_root(root_session)?;
    assert_eq!(approvals_after.len(), 0, "no pending approvals after stop");

    let approval = store.get_approval("apr-2c-test")?.expect("approval");
    // Emergency stop cancels approvals via record_decision("cancelled", ...)
    // The status field in the JSON is "cancelled", which does not map to Approved/Rejected,
    // so it stays None in the deserialized struct. The decided_by field proves cancellation.
    assert!(
        approval.decided_by.as_deref().unwrap_or("").contains(stop_id),
        "approval decided_by should reference stop_id, got: {:?}",
        approval.decided_by
    );
    assert!(
        approval.decided_at.is_some(),
        "approval should have decided_at set after emergency stop"
    );

    // Verify user interaction is cancelled
    let ui_after = store
        .get_user_interaction("ui-2c-approve")?
        .expect("user interaction");
    assert_eq!(ui_after.status, UserInteractionStatus::Cancelled);

    // Verify checkpoint is EmergencyStop
    let cp = load_latest_checkpoint(&config, root_session)?.expect("checkpoint");
    match &cp.yield_reason {
        YieldReason::EmergencyStop { stop_id: sid } => assert_eq!(sid, stop_id),
        other => panic!("expected EmergencyStop, got {:?}", other),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 2C.12: Restart reconciliation of stale active executions
// ---------------------------------------------------------------------------

#[serial_test::serial]
#[tokio::test]
async fn restart_reconciles_stale_active_executions() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = GatewayStore::open(&gateway_dir)?;

    let root_session = "root-2c-stale";
    let wf_id = "wf-stale";
    let ts_now = Utc::now().to_rfc3339();
    let ts_stale = (chrono::Utc::now() - chrono::Duration::seconds(300)).to_rfc3339();

    // Insert active executions: one stale (old heartbeat), one fresh
    store.upsert_active_execution(&ActiveExecutionRecord {
        execution_id: "exec-stale-1".to_string(),
        root_session_id: root_session.to_string(),
        workflow_id: Some(wf_id.to_string()),
        task_id: Some("task-stale".to_string()),
        session_id: format!("{root_session}/child-stale"),
        agent_id: "coder.default".to_string(),
        execution_kind: "workflow_task".to_string(),
        driver: None,
        pid: None,
        host_id: "test-host".to_string(),
        status: "running".to_string(),
        started_at: ts_stale.clone(),
        heartbeat_at: ts_stale.clone(), // 5 minutes ago — stale
        stop_requested_at: None,
        stopped_at: None,
        stop_id: None,
    })?;

    store.upsert_active_execution(&ActiveExecutionRecord {
        execution_id: "exec-stale-2".to_string(),
        root_session_id: root_session.to_string(),
        workflow_id: Some(wf_id.to_string()),
        task_id: Some("task-stale-2".to_string()),
        session_id: format!("{root_session}/child-stale-2"),
        agent_id: "coder.default".to_string(),
        execution_kind: "sandbox_process".to_string(),
        driver: Some("bubblewrap".to_string()),
        pid: Some(99999),
        host_id: "test-host".to_string(),
        status: "stop_requested".to_string(), // Also stale — should be reconciled
        started_at: ts_stale.clone(),
        heartbeat_at: ts_stale.clone(),
        stop_requested_at: Some(ts_stale.clone()),
        stopped_at: None,
        stop_id: Some("estop-test".to_string()),
    })?;

    store.upsert_active_execution(&ActiveExecutionRecord {
        execution_id: "exec-fresh".to_string(),
        root_session_id: root_session.to_string(),
        workflow_id: Some(wf_id.to_string()),
        task_id: Some("task-fresh".to_string()),
        session_id: format!("{root_session}/child-fresh"),
        agent_id: "coder.default".to_string(),
        execution_kind: "workflow_task".to_string(),
        driver: None,
        pid: None,
        host_id: "test-host".to_string(),
        status: "running".to_string(),
        started_at: ts_now.clone(),
        heartbeat_at: ts_now.clone(), // fresh
        stop_requested_at: None,
        stopped_at: None,
        stop_id: None,
    })?;

    // Call reconciliation — this happens on GatewayStore::open, but we can also
    // trigger it via the reconcile function indirectly. The store.open() already
    // calls it, so re-opening simulates a restart.
    let store_after = GatewayStore::open(&gateway_dir)?;

    // List active executions after reconciliation
    let execs = store_after.list_active_executions_for_root_sqlite(root_session)?;

    // Stale execution (running) should be marked "lost"
    let stale1 = execs.iter().find(|e| e.execution_id == "exec-stale-1").expect("stale-1");
    assert_eq!(stale1.status, "lost", "stale running exec should be marked lost");
    assert!(stale1.stopped_at.is_some(), "stale exec should have stopped_at");

    // Stale execution (stop_requested) should be marked "lost"
    let stale2 = execs.iter().find(|e| e.execution_id == "exec-stale-2").expect("stale-2");
    assert_eq!(stale2.status, "lost", "stale stop_requested exec should be marked lost");
    assert!(stale2.stopped_at.is_some());

    // Fresh execution should remain "running"
    let fresh = execs.iter().find(|e| e.execution_id == "exec-fresh").expect("fresh");
    assert_eq!(fresh.status, "running", "fresh exec should still be running");

    Ok(())
}

// ---------------------------------------------------------------------------
// 2C.13: Authorization matrix for emergency stop
// ---------------------------------------------------------------------------

fn write_emergency_manager_agent(agents_dir: &std::path::Path) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join("emergency-manager.default");
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
  id: "emergency-manager.default"
  name: "emergency-manager"
  description: "Dedicated emergency response agent"
capabilities:
  - type: "EmergencyStop"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Emergency Manager
You are the dedicated emergency response agent.
"#,
    )?;
    Ok(())
}

fn write_regular_agent(agents_dir: &std::path::Path) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join("regular-agent.default");
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
  id: "regular-agent.default"
  name: "regular-agent"
  description: "A regular agent without emergency stop capability"
capabilities:
  - type: "ReadAccess"
    scopes: ["*"]
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Regular Agent
You are a regular agent.
"#,
    )?;
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn emergency_stop_authorization_matrix() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    write_planner_agent(&workspace.agents_dir)?;
    write_emergency_manager_agent(&workspace.agents_dir)?;
    write_regular_agent(&workspace.agents_dir)?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    // --- Set up a minimal workflow so emergency_stop can proceed ---
    let root_session = "root-2c-auth";

    // Helper: create a workflow + task for a given root session
    fn setup_workflow_for_root(
        config: &autonoetic_types::config::GatewayConfig,
        store: &GatewayStore,
        root_session: &str,
    ) -> anyhow::Result<()> {
        let wf = ensure_workflow_for_root_session(
            config,
            Some(store),
            root_session,
            Some("planner.default"),
        )?;
        save_workflow_run(config, Some(store), &wf)?;
        let ts = Utc::now().to_rfc3339();
        let task = TaskRun {
            task_id: format!("{root_session}-task"),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "planner.default".to_string(),
            session_id: format!("{root_session}/self"),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
        };
        save_task_run(config, Some(store), &task)?;
        Ok(())
    }

    setup_workflow_for_root(&config, store.as_ref(), root_session)?;

    // 1. User call (no source_agent_id) — should succeed
    let user_root = format!("{root_session}-user");
    setup_workflow_for_root(&config, store.as_ref(), &user_root)?;
    let out_user = execution
        .emergency_stop_root_session(
            &user_root,
            "user-initiated stop",
            "user",
            "alice",
            "manual",
            None,
        )
        .await;
    assert!(out_user.is_ok(), "user call should succeed: {:?}", out_user.err());

    // 2. Gateway self-protection call — should succeed
    let gw_root = format!("{root_session}-gateway");
    setup_workflow_for_root(&config, store.as_ref(), &gw_root)?;
    let out_gateway = execution
        .emergency_stop_root_session(
            &gw_root,
            "security policy violation",
            "gateway",
            "security_policy",
            "security_policy",
            None,
        )
        .await;
    assert!(
        out_gateway.is_ok(),
        "gateway call should succeed: {:?}",
        out_gateway.err()
    );

    // 3. Emergency-manager agent with EmergencyStop capability — should succeed
    let em_root = format!("{root_session}-agent-allowed");
    setup_workflow_for_root(&config, store.as_ref(), &em_root)?;
    let out_agent_ok = execution
        .emergency_stop_root_session(
            &em_root,
            "agent-initiated stop",
            "agent",
            "emergency-manager.default",
            "manual",
            Some("emergency-manager.default"),
        )
        .await;
    assert!(
        out_agent_ok.is_ok(),
        "emergency-manager agent should succeed: {:?}",
        out_agent_ok.err()
    );

    // 4. Regular agent WITHOUT EmergencyStop capability — should be denied
    let reg_root = format!("{root_session}-agent-denied");
    setup_workflow_for_root(&config, store.as_ref(), &reg_root)?;
    let out_agent_denied = execution
        .emergency_stop_root_session(
            &reg_root,
            "unauthorized agent stop",
            "agent",
            "regular-agent.default",
            "manual",
            Some("regular-agent.default"),
        )
        .await;
    assert!(
        out_agent_denied.is_err(),
        "regular agent without EmergencyStop should be denied"
    );
    let err_msg = out_agent_denied.unwrap_err().to_string();
    assert!(
        err_msg.contains("Permission Denied"),
        "error should mention permission denied, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("regular-agent.default"),
        "error should name the agent, got: {}",
        err_msg
    );

    // 5. Non-existent agent — should fail with "not found"
    let missing_root = format!("{root_session}-missing");
    setup_workflow_for_root(&config, store.as_ref(), &missing_root)?;
    let out_agent_missing = execution
        .emergency_stop_root_session(
            &missing_root,
            "missing agent",
            "agent",
            "does-not-exist",
            "manual",
            Some("does-not-exist"),
        )
        .await;
    assert!(
        out_agent_missing.is_err(),
        "non-existent agent should fail"
    );

    Ok(())
}
