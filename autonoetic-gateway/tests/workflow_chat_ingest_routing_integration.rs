//! Phase 2B: User chat while workflow children run — planner receives `event.ingest` on root session.

mod support;

use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_task_run, save_task_run, save_workflow_run,
};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use chrono::Utc;
use support::{spawn_gateway_server, EnvGuard, JsonRpcClient, OpenAiStub, TestWorkspace};

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

#[serial_test::serial]
#[tokio::test]
async fn test_chat_ingest_from_child_session_routes_to_planner_root_while_tasks_run(
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
    };
    save_task_run(&config, None, &task)?;

    let (listen_addr, _server) = spawn_gateway_server(config.clone()).await?;
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
