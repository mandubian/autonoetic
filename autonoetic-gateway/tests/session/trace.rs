//! Integration test for multi-agent session trace reconstruction.


use crate::support::{
    read_causal_entries, seed_agent_revision, spawn_gateway_server_with_store, EnvGuard,
    JsonRpcClient, OpenAiStub, TestWorkspace,
};

fn install_parent_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
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
  description: "Parent agent that spawns child"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
capabilities:
  - type: "AgentSpawn"
    max_children: 5
  - type: "AgentMessage"
    patterns: ["*"]
---
# Parent Agent
When asked to delegate, spawn the child agent.
"#
        ),
    )?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn install_child_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
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
  description: "Child agent that does work"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
capabilities: []
---
# Child Agent
Reply with "Child completed task: <input>".
"#
        ),
    )?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

#[serial_test::serial]
#[test]
fn test_multi_agent_session_trace_reconstruction() -> anyhow::Result<()> {
    // #1090: multi-agent turns run the deep scheduler → router →
    // spawn_agent chain on tokio workers; the default 2 MiB worker stack
    // overflows in debug builds under plain `cargo test`. The big-stack
    // runtime mirrors the gateway binary's 8 MiB workers. Serial: this test
    // mutates the process-global LLM env vars, which must not overlap other
    // env-mutating tests (escalate) in the shared cargo-test process.
    crate::support::run_with_big_stack(test_multi_agent_session_trace_reconstruction_body)
}

async fn test_multi_agent_session_trace_reconstruction_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    let parent_id = "parent-agent";
    let child_id = "child-agent";

    install_parent_agent(&workspace.agents_dir.join(parent_id), parent_id)?;
    install_child_agent(&workspace.agents_dir.join(child_id), child_id)?;

    let stub = OpenAiStub::spawn(move |_, body_json| async move {
        let messages = body_json["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let latest_user = messages
            .iter()
            .rev()
            .find_map(|m| {
                if m["role"].as_str() == Some("user") {
                    m["content"].as_str()
                } else {
                    None
                }
            })
            .unwrap_or("");

        if latest_user.contains("delegate") {
            serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "agent_spawn",
                                "arguments": serde_json::json!({
                                    "intent": "delegate the work to the child agent",
                                    "agent_id": child_id,
                                    "message": "do work"
                                }).to_string()
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })
        } else {
            serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "Task completed" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })
        }
    })
    .await?;

    let _env = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let (server_addr, store, shutdown) = spawn_gateway_server_with_store(config.clone()).await?;
    let parent_rev = seed_agent_revision(
        &store,
        &config,
        parent_id,
        &workspace.agents_dir.join(parent_id),
    )?;
    let child_rev = seed_agent_revision(
        &store,
        &config,
        child_id,
        &workspace.agents_dir.join(child_id),
    )?;
    let mut client = JsonRpcClient::connect(server_addr).await?;

    let session_id = "session-multi-agent-test";

    let _response = client
        .event_ingest(
            "test-multi-1",
            parent_id,
            session_id,
            "test",
            "please delegate to child agent",
            None::<serde_json::Value>,
        )
        .await?;

    drop(shutdown);

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    let parent_rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(parent_id)
        .join(&parent_rev);
    let child_rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(child_id)
        .join(&child_rev);

    let gateway_causal_path = gateway_dir.join("history/causal_chain.jsonl");
    let parent_causal_path = parent_rev_dir.join("history/causal_chain.jsonl");
    let child_causal_path = child_rev_dir.join("history/causal_chain.jsonl");

    let trace_label = |entry: &autonoetic_types::causal_chain::CausalChainEntry| -> String {
        let tool = entry
            .payload
            .as_ref()
            .and_then(|p| p.get("tool_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if entry.category == "tool_invoke" && !tool.is_empty() {
            format!("{}/{} {}", entry.category, entry.action, tool)
        } else {
            format!("{}/{}", entry.category, entry.action)
        }
    };

    // The parent turn (LLM roundtrip → spawn → child turn → causal writes)
    // completes asynchronously on the gateway server task. Poll the causal
    // files for the spawn marker instead of a fixed sleep — the shared
    // cargo-test process runs this binary's tests in parallel, so wall-clock
    // deadlines race the pipeline (#1090).
    let collect_events = || -> anyhow::Result<Vec<(String, String)>> {
        let mut all_events: Vec<(String, String)> = Vec::new();
        if gateway_causal_path.exists() {
            let entries = read_causal_entries(&gateway_causal_path)?;
            for entry in entries {
                if entry.session_id == session_id {
                    all_events.push(("gateway".to_string(), trace_label(&entry)));
                }
            }
        }
        if parent_causal_path.exists() {
            let entries = read_causal_entries(&parent_causal_path)?;
            for entry in entries {
                if entry.session_id == session_id {
                    all_events.push((parent_id.to_string(), trace_label(&entry)));
                }
            }
        }
        if child_causal_path.exists() {
            let entries = read_causal_entries(&child_causal_path)?;
            for entry in entries {
                if entry.session_id == session_id {
                    all_events.push((child_id.to_string(), trace_label(&entry)));
                }
            }
        }
        Ok(all_events)
    };

    let mut all_events = Vec::new();
    for _ in 0..50 {
        all_events = collect_events()?;
        if all_events
            .iter()
            .any(|(_, action)| action.contains("spawn"))
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    tracing::info!(events = ?all_events, "Found events for session");

    assert!(!all_events.is_empty(), "Should have events in the session");

    let has_spawn = all_events
        .iter()
        .any(|(_, action)| action.contains("spawn"));
    assert!(has_spawn, "Session should contain spawn events");

    Ok(())
}

#[serial_test::serial]
#[test]
fn test_session_trace_deterministic_ordering() -> anyhow::Result<()> {
    // #1090: deep scheduler → router → spawn_agent chain overflows the
    // default 2 MiB `#[tokio::test]` stack in debug builds (SIGABRT under
    // plain `cargo test`); run on an explicit big-stack runtime instead.
    // Serial: mutates the process-global LLM env vars (see the sibling test).
    crate::support::run_with_big_stack(test_session_trace_deterministic_ordering_body)
}

async fn test_session_trace_deterministic_ordering_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    let agent_id = "simple-agent";

    std::fs::create_dir_all(workspace.agents_dir.join(agent_id))?;
    std::fs::write(
        workspace.agents_dir.join(agent_id).join("SKILL.md"),
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
  description: "Simple agent"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
capabilities: []
---
# Simple Agent
Reply with "Done".
"#
        ),
    )?;
    std::fs::write(
        workspace.agents_dir.join(agent_id).join("runtime.lock"),
        "dependencies: []",
    )?;

    let stub = OpenAiStub::spawn(|_, _| async move {
        serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "Done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    })
    .await?;

    let _env = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let (server_addr, store, _shutdown) = spawn_gateway_server_with_store(config.clone()).await?;
    let rev_id = seed_agent_revision(
        &store,
        &config,
        agent_id,
        &workspace.agents_dir.join(agent_id),
    )?;
    let mut client = JsonRpcClient::connect(server_addr).await?;

    let session_id = "session-deterministic-1";

    let _response = client
        .event_ingest(
            "test-2",
            agent_id,
            session_id,
            "test",
            "hello",
            None::<serde_json::Value>,
        )
        .await?;

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let rev_dir = workspace
        .agents_dir
        .join(".gateway")
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(&rev_id);
    let agent_causal_path = rev_dir.join("history/causal_chain.jsonl");

    let entries = read_causal_entries(&agent_causal_path)?;
    let session_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.session_id == session_id)
        .collect();

    assert!(
        !session_entries.is_empty(),
        "Should have events in the session"
    );

    let mut timestamps: Vec<&str> = session_entries
        .iter()
        .map(|e| e.timestamp.as_str())
        .collect();

    timestamps.sort();

    let is_sorted = timestamps.windows(2).all(|w| w[0] <= w[1]);
    assert!(is_sorted, "Events should be sorted by timestamp");

    let mut event_seqs: Vec<u64> = session_entries.iter().map(|e| e.event_seq).collect();
    event_seqs.sort();

    let seqs_sorted = event_seqs.windows(2).all(|w| w[0] <= w[1]);
    assert!(
        seqs_sorted,
        "Events should be sorted by event_seq within timestamp"
    );

    tracing::info!(
        timestamps = ?timestamps,
        event_seqs = ?event_seqs,
        "Deterministic ordering verified"
    );

    Ok(())
}
