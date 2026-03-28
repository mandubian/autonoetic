//! Integration test: Turn Continuation model for approval-gated tool calls.
//!
//! Verifies the full approval continuation flow:
//!   1. Agent calls `sandbox.exec` with a command containing remote access patterns.
//!   2. The tool handler returns `{approval_required: true}` and saves a `TurnContinuation`.
//!   3. `spawn_agent_once` returns `SpawnResult { suspended_for_approval: Some(request_id) }`.
//!   4. Operator approves the pending request.
//!   5. Second `spawn_agent_once` (same task_id) loads the continuation, executes the
//!      approved action, and resumes `execute_with_history` with the real tool result.
//!   6. The LLM's second call receives a `tool_result` message containing actual execution
//!      output — **not** `approval_required: true`.
//!   7. The continuation file is deleted after successful resume.

mod support;

use std::sync::{Arc, Mutex};

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::continuation::{
    continuations_dir, load_continuation, save_continuation,
};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::{
    approve_request, load_approval_requests, run_scheduler_tick, workflow_store,
};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRun, WorkflowRunStatus};
use support::{EnvGuard, OpenAiStub};

const LLM_BASE_URL_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_ENV: &str = "AUTONOETIC_LLM_API_KEY";

/// Command that contains `urllib` — triggers remote access detection in the
/// static analyser so the first `sandbox.exec` call requires approval.
/// When actually executed (with `approval_ref`), it merely prints a marker
/// string and makes no real network call.
const APPROVAL_TRIGGERING_COMMAND: &str =
    "python3 -c \"import urllib.request; print('exec-output-marker')\"";

fn install_exec_agent(agents_dir: &std::path::Path) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join("exec-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
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
  id: "exec-agent"
  name: "Exec Agent"
  description: "Runs shell commands via sandbox.exec."
capabilities:
  - type: "CodeExecution"
    patterns: ["*"]
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Exec Agent
Run commands and report output.
"#,
    )?;
    Ok(())
}

fn install_orchestrator_agent(agents_dir: &std::path::Path) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join("orchestrator-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
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
  id: "orchestrator-agent"
  name: "Orchestrator Agent"
  description: "Has workflow orchestration tools."
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
capabilities:
  - type: "AgentSpawn"
    max_children: 5
---
# Orchestrator Agent
Manage workflow tasks.
"#,
    )?;
    Ok(())
}

/// The LLM stub responds to two sequential requests:
///   - **Call 1**: returns a `sandbox.exec` tool call (no assistant text).
///   - **Call 2**: returns a final text reply acknowledging the exec output.
///
/// Both responses are pre-loaded into a shared queue so the stub can serve
/// them in order regardless of how many concurrent connections are opened.
fn make_stub_responses(command: &str) -> Vec<serde_json::Value> {
    let tool_call_response = serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "tc-exec-001",
                    "type": "function",
                    "function": {
                        "name": "sandbox.exec",
                        "arguments": serde_json::json!({ "command": command }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    });

    let final_text_response = serde_json::json!({
        "choices": [{
            "message": {
                "content": "Command executed successfully. Output received."
            },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 8 }
    });

    vec![tool_call_response, final_text_response]
}

#[serial_test::serial]
#[tokio::test]
async fn test_approval_continuation_suspends_and_resumes() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    // Ordered stub responses (call 1 = tool_call, call 2 = final reply).
    let responses = Arc::new(Mutex::new(make_stub_responses(APPROVAL_TRIGGERING_COMMAND)));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                // Fallback: empty stop response (should not be reached in this test)
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "cont-test-session";
    let task_id = "task-cont-test-001";

    // -----------------------------------------------------------------------
    // Phase 1: First spawn — should suspend at approval gate
    // -----------------------------------------------------------------------
    let first_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.",
            session_id,
            None,
            false,
            None,
            None,
            None, // no workflow_id (non-workflow path)
            Some(task_id),
        )
        .await?;

    // Turn must be suspended
    assert!(
        first_result.suspended_for_approval.is_some(),
        "expected turn to suspend at approval gate, got reply: {:?}",
        first_result.assistant_reply
    );
    let suspended_request_id = first_result.suspended_for_approval.unwrap();

    // Continuation file must exist on disk
    let cont_dir = continuations_dir(&config);
    let cont_file = cont_dir.join(format!("{}.json", task_id));
    assert!(
        cont_file.exists(),
        "continuation file should exist at {}",
        cont_file.display()
    );

    // Verify the saved continuation points to the right approval request
    let cont = load_continuation(&config, task_id)?.expect("continuation should be loadable");
    assert_eq!(
        cont.approval_request_id, suspended_request_id,
        "continuation approval_request_id should match suspended_for_approval"
    );
    assert_eq!(cont.pending_tool_call.tool_name, "sandbox.exec");

    // -----------------------------------------------------------------------
    // Phase 2: Approve the pending request
    // -----------------------------------------------------------------------
    let pending = load_approval_requests(&config, Some(store.as_ref()))?;
    assert_eq!(
        pending.len(),
        1,
        "expected exactly 1 pending approval, got {}",
        pending.len()
    );
    assert_eq!(pending[0].request_id, suspended_request_id);

    approve_request(
        &config,
        Some(store.as_ref()),
        &suspended_request_id,
        "test-operator",
        Some("approved for test".to_string()),
    )?;

    // -----------------------------------------------------------------------
    // Phase 3: Second spawn — should resume from continuation
    // -----------------------------------------------------------------------
    let second_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.", // original message (not used on resume)
            session_id,
            None,
            false,
            None,
            None,
            None,
            Some(task_id),
        )
        .await?;

    // Turn must complete (not suspend again)
    assert!(
        second_result.suspended_for_approval.is_none(),
        "resumed turn should not suspend again"
    );
    assert!(
        second_result.assistant_reply.is_some(),
        "resumed turn should produce an assistant reply"
    );

    // Continuation file must be deleted after successful resume
    assert!(
        !cont_file.exists(),
        "continuation file should be deleted after successful resume"
    );

    // -----------------------------------------------------------------------
    // Phase 4: Verify LLM history integrity
    //
    // The second LLM call (call 2) must have a `tool_result` message in the
    // request body whose content does NOT contain `approval_required: true`.
    // This confirms the agent saw the real exec output, not the approval gate.
    // -----------------------------------------------------------------------
    let captured = stub.captured_bodies();
    // There should be exactly 2 LLM calls: one during suspension, one on resume.
    assert_eq!(
        captured.len(),
        2,
        "expected exactly 2 LLM calls (suspend + resume), got {}",
        captured.len()
    );

    let resume_call_messages = captured[1]
        .get("messages")
        .and_then(|m| m.as_array())
        .expect("second LLM call should have messages array");

    // Find the tool_result message for sandbox.exec
    let tool_result_msg = resume_call_messages
        .iter()
        .find(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("tool"));
    assert!(
        tool_result_msg.is_some(),
        "second LLM call must include a tool_result message"
    );

    let tool_content = tool_result_msg
        .unwrap()
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("");

    // The real result must NOT be the approval gate response
    assert!(
        !tool_content.contains("\"approval_required\":true")
            && !tool_content.contains("approval_required"),
        "tool_result in resumed call must not contain approval_required — got: {}",
        tool_content
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_approval_continuation_file_deleted_on_cancellation() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let responses = Arc::new(Mutex::new(make_stub_responses(APPROVAL_TRIGGERING_COMMAND)));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "fallback"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "cancel-test-session";
    let task_id = "task-cancel-test-001";

    // Suspend the turn
    let first_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the command.",
            session_id,
            None,
            false,
            None,
            None,
            None,
            Some(task_id),
        )
        .await?;

    assert!(first_result.suspended_for_approval.is_some());

    let cont_file = continuations_dir(&config).join(format!("{}.json", task_id));
    assert!(
        cont_file.exists(),
        "continuation file should exist before cancellation"
    );

    // Cancel via delete_continuation directly (simulating workflow.cancel_task)
    autonoetic_gateway::runtime::continuation::delete_continuation(&config, task_id)?;

    assert!(
        !cont_file.exists(),
        "continuation file should be gone after cancellation"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_parallel_join_waits_for_approval_task_completion() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;
    install_orchestrator_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-parallel-join";
    let root_session_id = "workflow-root-parallel-join";
    let approval_task_id = "task-parallel-approval-001";
    let fast_task_id = "task-parallel-fast-001";
    let approval_session_id = "workflow-root-parallel-join/exec-agent-approval-001";
    let fast_session_id = "workflow-root-parallel-join/exec-agent-fast-001";

    let workflow = WorkflowRun {
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: "orchestrator-agent".to_string(),
        status: WorkflowRunStatus::WaitingChildren,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        active_task_ids: vec![],
        queued_task_ids: vec![],
        join_policy: Default::default(),
        join_task_ids: vec![approval_task_id.to_string(), fast_task_id.to_string()],
    };
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    let approval_task = TaskRun {
        task_id: approval_task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "exec-agent".to_string(),
        session_id: approval_session_id.to_string(),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("orchestrator-agent".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Run the approval-gated command.".to_string()),
        metadata: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &approval_task)?;

    let fast_task = TaskRun {
        task_id: fast_task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: "exec-agent".to_string(),
        session_id: fast_session_id.to_string(),
        parent_session_id: root_session_id.to_string(),
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("orchestrator-agent".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Run fast task.".to_string()),
        metadata: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &fast_task)?;

    let responses = Arc::new(Mutex::new(make_stub_responses(APPROVAL_TRIGGERING_COMMAND)));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    // Task A suspends on approval.
    let first_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the approval-gated command.",
            approval_session_id,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(approval_task_id),
        )
        .await?;
    let suspended_request_id = first_result
        .suspended_for_approval
        .expect("approval task should suspend");

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        approval_task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting operator approval".to_string()),
    )?;

    // Task B completes while Task A is still awaiting approval.
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        fast_task_id,
        TaskRunStatus::Succeeded,
        Some("Fast path completed".to_string()),
    )?;

    // Planner view: join must still be unsatisfied.
    let (orchestrator_manifest, orchestrator_dir) =
        execution.load_agent_manifest("orchestrator-agent")?;
    let policy = PolicyEngine::new(orchestrator_manifest.clone());
    let registry = default_registry();
    let wait_args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [approval_task_id, fast_task_id],
        "timeout_secs": 0
    });
    let wait_before_raw = registry.execute(
        "workflow.wait",
        &orchestrator_manifest,
        &policy,
        &orchestrator_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&wait_args)?,
        Some(root_session_id),
        Some("turn-wait-before"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let wait_before: serde_json::Value = serde_json::from_str(&wait_before_raw)?;
    assert_eq!(
        wait_before.get("join_satisfied").and_then(|v| v.as_bool()),
        Some(false),
        "join should stay unsatisfied while one join task awaits approval"
    );

    let wait_before_tasks = wait_before
        .get("tasks")
        .and_then(|v| v.as_array())
        .expect("workflow.wait should return task list");
    let approval_status_before = wait_before_tasks
        .iter()
        .find(|t| t.get("task_id").and_then(|v| v.as_str()) == Some(approval_task_id))
        .and_then(|t| t.get("status"))
        .and_then(|v| v.as_str());
    let fast_status_before = wait_before_tasks
        .iter()
        .find(|t| t.get("task_id").and_then(|v| v.as_str()) == Some(fast_task_id))
        .and_then(|t| t.get("status"))
        .and_then(|v| v.as_str());
    assert_eq!(approval_status_before, Some("AwaitingApproval"));
    assert_eq!(fast_status_before, Some("Succeeded"));

    let wf_before = workflow_store::load_workflow_run(&config, Some(store.as_ref()), workflow_id)?
        .expect("workflow should exist");
    assert_ne!(
        wf_before.status,
        WorkflowRunStatus::Resumable,
        "workflow must not be resumable until all join tasks are terminal"
    );

    // Approve task A and resume it.
    approve_request(
        &config,
        Some(store.as_ref()),
        &suspended_request_id,
        "test-operator",
        Some("approved for parallel-join test".to_string()),
    )?;
    let resumed_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the approval-gated command.",
            approval_session_id,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(approval_task_id),
        )
        .await?;
    assert!(
        resumed_result.suspended_for_approval.is_none(),
        "approved task should resume and finish"
    );

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        approval_task_id,
        TaskRunStatus::Succeeded,
        Some("Approved path completed".to_string()),
    )?;

    // Planner view after resume: join now satisfied.
    let wait_after_raw = registry.execute(
        "workflow.wait",
        &orchestrator_manifest,
        &policy,
        &orchestrator_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&wait_args)?,
        Some(root_session_id),
        Some("turn-wait-after"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let wait_after: serde_json::Value = serde_json::from_str(&wait_after_raw)?;
    assert_eq!(
        wait_after.get("join_satisfied").and_then(|v| v.as_bool()),
        Some(true),
        "join should be satisfied once the approved task also reaches terminal state"
    );

    let wf_after = workflow_store::load_workflow_run(&config, Some(store.as_ref()), workflow_id)?
        .expect("workflow should exist");
    assert_eq!(wf_after.status, WorkflowRunStatus::Resumable);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_approval_timeout_fails_task_and_satisfies_join() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.approval_timeout_secs = 1;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-timeout-e2e";
    let root_session_id = "workflow-root-timeout-e2e";
    let task_id = "task-timeout-e2e-001";
    let child_session_id = "workflow-root-timeout-e2e/exec-agent-001";

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
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Run the data fetch command.".to_string()),
        metadata: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let responses = Arc::new(Mutex::new(vec![make_stub_responses(
        APPROVAL_TRIGGERING_COMMAND,
    )
    .into_iter()
    .next()
    .expect("stub response should exist")]));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    // Suspend task by hitting the approval boundary.
    let first_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.",
            child_session_id,
            None,
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_id),
        )
        .await?;
    assert!(
        first_result.suspended_for_approval.is_some(),
        "task should suspend for approval before timeout handling"
    );

    let cont_file = continuations_dir(&config).join(format!("{}.json", task_id));
    assert!(
        cont_file.exists(),
        "continuation file should exist before timeout handling"
    );

    // Mirror scheduler path: suspended tasks are tracked as AwaitingApproval.
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting operator approval".to_string()),
    )?;

    // Force continuation to appear stale so timeout logic triggers immediately.
    let mut continuation =
        load_continuation(&config, task_id)?.expect("continuation should exist for timeout test");
    continuation.suspended_at = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    save_continuation(&config, task_id, &continuation)?;

    run_scheduler_tick(execution.clone()).await?;

    assert!(
        cont_file.exists(),
        "continuation file should be PRESERVED after timeout (can resume if approval granted later)"
    );

    let timed_out_task =
        workflow_store::load_task_run(&config, Some(store.as_ref()), workflow_id, task_id)?
            .expect("timed out task should still exist");
    assert_eq!(timed_out_task.status, TaskRunStatus::Failed);
    assert_eq!(
        timed_out_task.result_summary.as_deref(),
        Some("Approval timed out")
    );

    let updated_workflow =
        workflow_store::load_workflow_run(&config, Some(store.as_ref()), workflow_id)?
            .expect("workflow should exist");
    assert_eq!(
        updated_workflow.status,
        WorkflowRunStatus::Resumable,
        "join should be satisfied after timed-out terminal task"
    );

    let events = workflow_store::load_workflow_events(&config, Some(store.as_ref()), workflow_id)?;
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "workflow.join.satisfied"),
        "expected workflow.join.satisfied event after timeout failure"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_restart_during_suspension_then_approve_and_resume() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let workflow_id = "wf-restart-e2e";
    let root_session_id = "workflow-root-restart-e2e";
    let task_id = "task-restart-e2e-001";
    let child_session_id = "workflow-root-restart-e2e/exec-agent-001";

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
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Run the data fetch command.".to_string()),
        metadata: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let responses = Arc::new(Mutex::new(make_stub_responses(APPROVAL_TRIGGERING_COMMAND)));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    // "Before restart": first gateway instance suspends on approval.
    let execution_before = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    let first_result = execution_before
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.",
            child_session_id,
            None,
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_id),
        )
        .await?;
    let request_id = first_result
        .suspended_for_approval
        .expect("task should suspend before restart");
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting operator approval".to_string()),
    )?;
    let cont_file = continuations_dir(&config).join(format!("{}.json", task_id));
    assert!(
        cont_file.exists(),
        "continuation should exist before restart"
    );

    // Simulate restart: new execution service + reopened store.
    let store_after_restart =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    let execution_after = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store_after_restart.clone()),
    ));

    approve_request(
        &config,
        Some(store_after_restart.as_ref()),
        &request_id,
        "test-operator",
        Some("approved after restart".to_string()),
    )?;

    // "After restart": continuation is loaded and resumed successfully.
    let resumed = execution_after
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.",
            child_session_id,
            None,
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_id),
        )
        .await?;
    assert!(
        resumed.suspended_for_approval.is_none(),
        "resumed task should complete after restart + approval"
    );
    assert!(
        resumed.assistant_reply.is_some(),
        "resumed task should return assistant reply"
    );
    assert!(
        !cont_file.exists(),
        "continuation file should be deleted after resumed completion"
    );

    workflow_store::update_task_run_status(
        &config,
        Some(store_after_restart.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::Succeeded,
        Some("Completed after restart".to_string()),
    )?;

    let wf_after = workflow_store::load_workflow_run(
        &config,
        Some(store_after_restart.as_ref()),
        workflow_id,
    )?
    .expect("workflow should exist");
    assert_eq!(
        wf_after.status,
        WorkflowRunStatus::Resumable,
        "join should be satisfied after resumed completion"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_two_approval_tasks_both_resume_before_join_satisfies() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;
    install_orchestrator_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-dual-approval-e2e";
    let root_session_id = "workflow-root-dual-approval-e2e";
    let task_a = "task-dual-approval-a";
    let task_b = "task-dual-approval-b";
    let session_a = "workflow-root-dual-approval-e2e/exec-agent-a";
    let session_b = "workflow-root-dual-approval-e2e/exec-agent-b";

    let workflow = WorkflowRun {
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: "orchestrator-agent".to_string(),
        status: WorkflowRunStatus::WaitingChildren,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        active_task_ids: vec![],
        queued_task_ids: vec![],
        join_policy: Default::default(),
        join_task_ids: vec![task_a.to_string(), task_b.to_string()],
    };
    workflow_store::save_workflow_run(&config, Some(store.as_ref()), &workflow)?;

    for (task_id, session_id) in [(task_a, session_a), (task_b, session_b)] {
        let task = TaskRun {
            task_id: task_id.to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "exec-agent".to_string(),
            session_id: session_id.to_string(),
            parent_session_id: root_session_id.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("orchestrator-agent".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("Run approval command.".to_string()),
            metadata: None,
        };
        workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;
    }

    let template = make_stub_responses(APPROVAL_TRIGGERING_COMMAND);
    let tool_call_response = template[0].clone();
    let final_response = template[1].clone();
    let responses = Arc::new(Mutex::new(vec![
        tool_call_response.clone(),
        tool_call_response,
        final_response.clone(),
        final_response,
    ]));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    // Both tasks suspend and wait for approval.
    let suspended_a = execution
        .spawn_agent_once(
            "exec-agent",
            "Run approval command A.",
            session_a,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_a),
        )
        .await?
        .suspended_for_approval
        .expect("task A should suspend");
    let suspended_b = execution
        .spawn_agent_once(
            "exec-agent",
            "Run approval command B.",
            session_b,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_b),
        )
        .await?
        .suspended_for_approval
        .expect("task B should suspend");

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_a,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting approval A".to_string()),
    )?;
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_b,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting approval B".to_string()),
    )?;

    approve_request(
        &config,
        Some(store.as_ref()),
        &suspended_a,
        "test-operator",
        Some("approved A".to_string()),
    )?;
    approve_request(
        &config,
        Some(store.as_ref()),
        &suspended_b,
        "test-operator",
        Some("approved B".to_string()),
    )?;

    // Resume A first; join should still be unsatisfied.
    let resumed_a = execution
        .spawn_agent_once(
            "exec-agent",
            "Run approval command A.",
            session_a,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_a),
        )
        .await?;
    assert!(resumed_a.suspended_for_approval.is_none());
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_a,
        TaskRunStatus::Succeeded,
        Some("Task A completed".to_string()),
    )?;

    let (orchestrator_manifest, orchestrator_dir) =
        execution.load_agent_manifest("orchestrator-agent")?;
    let policy = PolicyEngine::new(orchestrator_manifest.clone());
    let registry = default_registry();
    let wait_args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 0
    });
    let wait_mid_raw = registry.execute(
        "workflow.wait",
        &orchestrator_manifest,
        &policy,
        &orchestrator_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&wait_args)?,
        Some(root_session_id),
        Some("turn-wait-mid"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let wait_mid: serde_json::Value = serde_json::from_str(&wait_mid_raw)?;
    assert_eq!(
        wait_mid.get("join_satisfied").and_then(|v| v.as_bool()),
        Some(false),
        "join must remain unsatisfied until both approval tasks finish"
    );

    // Resume B; now join should be satisfied.
    let resumed_b = execution
        .spawn_agent_once(
            "exec-agent",
            "Run approval command B.",
            session_b,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_b),
        )
        .await?;
    assert!(resumed_b.suspended_for_approval.is_none());
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_b,
        TaskRunStatus::Succeeded,
        Some("Task B completed".to_string()),
    )?;

    let wait_end_raw = registry.execute(
        "workflow.wait",
        &orchestrator_manifest,
        &policy,
        &orchestrator_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&wait_args)?,
        Some(root_session_id),
        Some("turn-wait-end"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let wait_end: serde_json::Value = serde_json::from_str(&wait_end_raw)?;
    assert_eq!(
        wait_end.get("join_satisfied").and_then(|v| v.as_bool()),
        Some(true),
        "join should satisfy once both approval tasks complete"
    );

    let wf_after = workflow_store::load_workflow_run(&config, Some(store.as_ref()), workflow_id)?
        .expect("workflow should exist");
    assert_eq!(wf_after.status, WorkflowRunStatus::Resumable);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_workflow_cancel_task_cancels_suspended_task_and_satisfies_join() -> anyhow::Result<()>
{
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_exec_agent(&workspace.agents_dir)?;
    install_orchestrator_agent(&workspace.agents_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let workflow_id = "wf-cancel-e2e";
    let root_session_id = "workflow-root-cancel-e2e";
    let task_id = "task-cancel-e2e-001";
    let child_session_id = "workflow-root-cancel-e2e/exec-agent-001";

    let workflow = WorkflowRun {
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: "orchestrator-agent".to_string(),
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
        status: TaskRunStatus::Running,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("orchestrator-agent".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("Run the data fetch command.".to_string()),
        metadata: None,
    };
    workflow_store::save_task_run(&config, Some(store.as_ref()), &task)?;

    let responses = Arc::new(Mutex::new(vec![make_stub_responses(
        APPROVAL_TRIGGERING_COMMAND,
    )
    .into_iter()
    .next()
    .expect("stub response should exist")]));
    let responses_clone = Arc::clone(&responses);
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    // Suspend task by hitting the approval boundary.
    let first_result = execution
        .spawn_agent_once(
            "exec-agent",
            "Run the data fetch command.",
            child_session_id,
            Some("orchestrator-agent"),
            false,
            None,
            None,
            Some(workflow_id),
            Some(task_id),
        )
        .await?;
    assert!(
        first_result.suspended_for_approval.is_some(),
        "task should suspend for approval before cancellation"
    );

    let cont_file = continuations_dir(&config).join(format!("{}.json", task_id));
    assert!(
        cont_file.exists(),
        "continuation file should exist before workflow.cancel_task"
    );

    // Mirror scheduler behavior: suspended tasks are marked AwaitingApproval.
    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_id,
        TaskRunStatus::AwaitingApproval,
        Some("Awaiting operator approval".to_string()),
    )?;

    // Invoke workflow.cancel_task through the native tool registry.
    let (orchestrator_manifest, orchestrator_dir) =
        execution.load_agent_manifest("orchestrator-agent")?;
    let policy = PolicyEngine::new(orchestrator_manifest.clone());
    let registry = default_registry();
    let cancel_args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_id": task_id,
        "reason": "Cancelled by integration test"
    });
    let cancel_raw = registry.execute(
        "workflow.cancel_task",
        &orchestrator_manifest,
        &policy,
        &orchestrator_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&cancel_args)?,
        Some(root_session_id),
        Some("turn-cancel-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let cancel_json: serde_json::Value = serde_json::from_str(&cancel_raw)?;
    assert_eq!(
        cancel_json.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "workflow.cancel_task should succeed"
    );
    assert_eq!(
        cancel_json.get("status").and_then(|v| v.as_str()),
        Some("Cancelled")
    );

    assert!(
        !cont_file.exists(),
        "continuation file should be deleted by workflow.cancel_task"
    );

    let cancelled_task =
        workflow_store::load_task_run(&config, Some(store.as_ref()), workflow_id, task_id)?
            .expect("task should still exist after cancellation");
    assert_eq!(cancelled_task.status, TaskRunStatus::Cancelled);
    assert_eq!(
        cancelled_task.result_summary.as_deref(),
        Some("Cancelled by integration test")
    );

    let updated_workflow =
        workflow_store::load_workflow_run(&config, Some(store.as_ref()), workflow_id)?
            .expect("workflow should exist");
    assert_eq!(
        updated_workflow.status,
        WorkflowRunStatus::Resumable,
        "join should be satisfied after cancelled terminal task"
    );

    let events = workflow_store::load_workflow_events(&config, Some(store.as_ref()), workflow_id)?;
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "workflow.join.satisfied"),
        "expected workflow.join.satisfied event after cancellation"
    );

    Ok(())
}
