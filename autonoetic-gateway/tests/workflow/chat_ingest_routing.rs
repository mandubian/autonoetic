//! Phase 2B: User chat while workflow children run — planner receives `event.ingest` on root session.



use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_task_run, save_task_run, save_workflow_run,
};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use chrono::Utc;
use crate::support::{
    seed_agent_revision, spawn_gateway_server_with_store, EnvGuard, JsonRpcClient, OpenAiStub,
    TestWorkspace,
};

fn write_minimal_reasoning_agent(
    agents_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        format!(
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
  id: "{agent_id}"
  name: "{agent_id}"
  description: "test"
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Test
"#
        ),
    )?;
    Ok(())
}

// This test drives a live JSON-RPC ingress that runs a full planner turn
// (router → executor → reqwest/hyper LLM stub call). In debug builds the
// combined future depth overflows the default 2 MiB test-thread stack; it
// passes under --release and at RUST_MIN_STACK=4 MiB. `#[tokio::test]`
// doesn't expose `thread_stack_size`, so — same pattern as
// gateway_ingress_integration (#836) — the runtime runs on an OS thread
// with an 8 MiB stack. Debug-only stack depth, not a production bug.
#[serial_test::serial]
#[test]
fn test_chat_ingest_from_child_session_routes_to_planner_root_while_tasks_run(
) -> anyhow::Result<()> {
    let child = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(chat_ingest_from_child_session_routes_to_planner_root_while_tasks_run_impl())
        })?;
    child.join().expect("test thread panicked")
}

async fn chat_ingest_from_child_session_routes_to_planner_root_while_tasks_run_impl(
) -> anyhow::Result<()> {
    let stub = OpenAiStub::spawn(|_, _body_json| async move {
        serde_json::json!({
            "id": "chatcmpl-2b13",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Planner ack." },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    })
    .await?;

    let _guard_url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _guard_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agents_dir = &workspace.agents_dir;

    write_minimal_reasoning_agent(agents_dir, "planner.default")?;
    write_minimal_reasoning_agent(agents_dir, "coder.default")?;

    let ts = Utc::now().to_rfc3339();
    let root_session = "root-2b13-chat-route";
    let child_session = "root-2b13-chat-route/delegation-coder";

    let mut wf =
        ensure_workflow_for_root_session(&config, None, root_session, Some("planner.default"))?;
    wf.status = WorkflowRunStatus::WaitingChildren;
    wf.join_task_ids = vec!["task-2b13".to_string()];
    wf.updated_at = ts.clone();
    save_workflow_run(&config, None, &wf)?;

    let task = TaskRun {
        task_id: "task-2b13".to_string(),
        workflow_id: wf.workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: child_session.to_string(),
        parent_session_id: root_session.to_string(),
        status: TaskRunStatus::Running,
        created_at: ts.clone(),
        updated_at: ts,
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: Some("main".to_string()),
        message: None,
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(&config, None, &task)?;

    let (listen_addr, store, _server) = spawn_gateway_server_with_store(config.clone()).await?;
    seed_agent_revision(
        &store,
        &config,
        "planner.default",
        &agents_dir.join("planner.default"),
    )?;
    seed_agent_revision(
        &store,
        &config,
        "coder.default",
        &agents_dir.join("coder.default"),
    )?;
    let mut client = JsonRpcClient::connect(listen_addr).await?;

    let user_line = "User update while parallel work runs";
    let resp = client
        .event_ingest("1", "coder.default", child_session, "chat", user_line, None)
        .await?;

    assert!(
        resp.error.is_none(),
        "event.ingest failed: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    assert_eq!(result["session_id"], root_session);
    assert_eq!(result["target_agent_id"], "planner.default");

    let bodies = stub.captured_bodies();
    let last = bodies.last().expect("stub should see LLM request");
    let payload = last.to_string();
    assert!(
        payload.contains(user_line),
        "planner completion request should include user text (wrapped in gateway ingest prefix); payload snippet: {}",
        &payload[..payload.len().min(500)]
    );

    let task_after = load_task_run(
        &workspace.gateway_config(),
        None,
        &wf.workflow_id,
        "task-2b13",
    )?
    .expect("task");
    assert_eq!(
        task_after.status,
        TaskRunStatus::Running,
        "child task must keep running (not cancelled by user chat)"
    );

    Ok(())
}

/// Regression (observed 2026-09-04, workflow wf-62ee6c3b): a DM sent to a
/// spawned workflow child was never answered. The delivery pump delivers the
/// wake as `event.ingest {event_type: "chat", session_id: <child>}` with
/// `metadata.signal_type = "agent_message"`; the active-workflow chat reroute
/// attracted it to the root planner session, so the parked recipient was never
/// woken and its `agent_message_deliveries` row stayed `delivered_at = NULL`.
/// The wake must reach the addressed session; only human/agent-authored chat
/// is rerouted to the planner.
#[serial_test::serial]
#[test]
fn test_agent_message_wake_to_workflow_child_session_is_not_rerouted() -> anyhow::Result<()> {
    let child = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(agent_message_wake_to_workflow_child_session_is_not_rerouted_impl())
        })?;
    child.join().expect("test thread panicked")
}

async fn agent_message_wake_to_workflow_child_session_is_not_rerouted_impl(
) -> anyhow::Result<()> {
    let stub = OpenAiStub::spawn(|_, _body_json| async move {
        serde_json::json!({
            "id": "chatcmpl-dm-wake",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ack" },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    })
    .await?;

    let _guard_url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _guard_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agents_dir = &workspace.agents_dir;

    write_minimal_reasoning_agent(agents_dir, "planner.default")?;
    write_minimal_reasoning_agent(agents_dir, "coder.default")?;

    let ts = Utc::now().to_rfc3339();
    let root_session = "root-dm-wake-route";
    let child_session = "root-dm-wake-route/delegation-coder";

    let mut wf =
        ensure_workflow_for_root_session(&config, None, root_session, Some("planner.default"))?;
    wf.status = WorkflowRunStatus::WaitingChildren;
    wf.join_task_ids = vec!["task-dm-wake".to_string()];
    wf.updated_at = ts.clone();
    save_workflow_run(&config, None, &wf)?;

    let task = TaskRun {
        task_id: "task-dm-wake".to_string(),
        workflow_id: wf.workflow_id.clone(),
        agent_id: "coder.default".to_string(),
        session_id: child_session.to_string(),
        parent_session_id: root_session.to_string(),
        status: TaskRunStatus::Running,
        created_at: ts.clone(),
        updated_at: ts,
        source_agent_id: Some("planner.default".to_string()),
        result_summary: None,
        join_group: Some("main".to_string()),
        message: None,
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    save_task_run(&config, None, &task)?;

    let (listen_addr, store, _server) = spawn_gateway_server_with_store(config.clone()).await?;
    seed_agent_revision(
        &store,
        &config,
        "planner.default",
        &agents_dir.join("planner.default"),
    )?;
    seed_agent_revision(
        &store,
        &config,
        "coder.default",
        &agents_dir.join("coder.default"),
    )?;
    let mut client = JsonRpcClient::connect(listen_addr).await?;

    // 1. Pump-delivered `agent_message` wake notice → must be delivered to the
    //    addressed child session, not attracted to the workflow root.
    let wake_text = "[Gateway] Wake-up: direct message msg-regression from agent 'planner.default' (session root-dm-wake-route).";
    let resp = client
        .event_ingest(
            "1",
            "coder.default",
            child_session,
            "chat",
            wake_text,
            Some(serde_json::json!({
                "sender_id": "gateway-signal-poller",
                "channel_id": "signal-poller-root-dm-wake-route/delegation-coder",
                "signal_delivered": true,
                "signal_request_id": "notif-dm-regression",
                "signal_type": "agent_message",
                "approval_request_id": null,
                "approval_status": null,
            })),
        )
        .await?;
    assert!(
        resp.error.is_none(),
        "agent_message wake ingest failed: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    assert_eq!(
        result["session_id"], child_session,
        "DM wake must stay on the addressed child session"
    );
    assert_eq!(
        result["target_agent_id"], "coder.default",
        "DM wake must target the addressed recipient agent"
    );
    let bodies = stub.captured_bodies();
    assert!(
        bodies
            .iter()
            .any(|b| b.to_string().contains("msg-regression")),
        "recipient agent's completion request should carry the wake text; got {} body(ies)",
        bodies.len()
    );

    // 2. Pump-delivered signal that is NOT an agent_message wake (e.g. approval
    //    resolution) keeps the existing reroute-to-planner behavior — the fix
    //    is scoped narrowly to DM wakes.
    let resp = client
        .event_ingest(
            "2",
            "coder.default",
            child_session,
            "chat",
            "approval resolved",
            Some(serde_json::json!({
                "sender_id": "gateway-signal-poller",
                "signal_delivered": true,
                "signal_request_id": "notif-approval-regression",
                "approval_request_id": "apr-regression",
                "approval_status": "approved",
            })),
        )
        .await?;
    assert!(
        resp.error.is_none(),
        "approval signal ingest failed: {:?}",
        resp.error
    );
    let result = resp.result.expect("result");
    assert_eq!(
        result["session_id"], root_session,
        "non-DM signal chat keeps the user-chat reroute to the workflow root"
    );
    assert_eq!(result["target_agent_id"], "planner.default");

    Ok(())
}
