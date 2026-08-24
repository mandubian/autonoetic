//! Integration tests for lenient deserialization helpers that help
//! weak function-calling models (e.g. tencent/hy3-preview:free)
//! avoid schema errors on common type mismatches.


use autonoetic_gateway::router::JsonRpcRequest;
use crate::support::{
    seed_agent_revision, spawn_gateway_server_with_store, EnvGuard, JsonRpcClient, OpenAiStub,
    TestWorkspace,
};

async fn send_tool(
    client: &mut JsonRpcClient,
    id: impl Into<String>,
    method: &str,
    params: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    client
        .send(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: method.to_string(),
            params,
            auth_token: std::env::var("AUTONOETIC_SHARED_SECRET").ok(),
        })
        .await?;
    let resp = client.recv().await?;
    if let Some(err) = resp.error {
        Ok(serde_json::json!({ "error": err }))
    } else {
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";

fn install_target_agent_with_schema(
    agent_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<()> {
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
  description: "Target agent with input schema"
io:
  accepts:
    type: object
    required: ["location", "date"]
    properties:
      location:
        type: string
      date:
        type: string
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---
# Target Agent
Reply with "Done".
"#
        ),
    )?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

#[test]
#[serial_test::serial]
fn test_agent_spawn_message_object_coerced_to_string() -> anyhow::Result<()> {
    // #1090: the spawn/LLM chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(test_agent_spawn_message_object_coerced_to_string_body)
}

async fn test_agent_spawn_message_object_coerced_to_string_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: workspace.agents_dir.join(".gateway"),
        agents_dir: workspace.agents_dir.clone(),
        ..workspace.gateway_config()
    };

    let target_id = "target-weather";
    install_target_agent_with_schema(&workspace.agents_dir.join(target_id), target_id)?;

    let stub = OpenAiStub::spawn(|_, _| async move {
        serde_json::json!({
            "choices": [{
                "message": { "content": "Done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    })
    .await?;

    let _env = EnvGuard::set(LLM_BASE_URL_OVERRIDE_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_OVERRIDE_ENV, "test-key");

    let (server_addr, store, _shutdown) = spawn_gateway_server_with_store(config.clone()).await?;
    seed_agent_revision(
        &store,
        &config,
        target_id,
        &workspace.agents_dir.join(target_id),
    )?;

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut client = JsonRpcClient::connect(server_addr).await?;

    // The model passes `message` as an OBJECT instead of a JSON-encoded string.
    // This is the exact failure mode from tencent/hy3-preview:free.
    let payload = serde_json::json!({
        "agent_id": target_id,
        "message": { "location": "Paris", "date": "tomorrow" }
    });

    let result = send_tool(&mut client, "test-spawn-object-msg", "agent_spawn", payload).await?;

    assert!(
        result.get("error").is_none(),
        "agent.spawn should coerce object message to string: {:?}",
        result.get("error")
    );

    Ok(())
}

#[test]
#[serial_test::serial]
fn test_agent_spawn_async_string_bool_coerced() -> anyhow::Result<()> {
    // #1090: see test_agent_spawn_message_object_coerced_to_string.
    crate::support::run_with_big_stack(test_agent_spawn_async_string_bool_coerced_body)
}

async fn test_agent_spawn_async_string_bool_coerced_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: workspace.agents_dir.join(".gateway"),
        agents_dir: workspace.agents_dir.clone(),
        ..workspace.gateway_config()
    };

    let target_id = "target-async-bool";
    install_target_agent_with_schema(&workspace.agents_dir.join(target_id), target_id)?;

    let stub = OpenAiStub::spawn(|_, _| async move {
        serde_json::json!({
            "choices": [{
                "message": { "content": "Done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    })
    .await?;

    let _env = EnvGuard::set(LLM_BASE_URL_OVERRIDE_ENV, stub.completion_url());
    let _key = EnvGuard::set(LLM_API_KEY_OVERRIDE_ENV, "test-key");

    let (server_addr, store, _shutdown) = spawn_gateway_server_with_store(config.clone()).await?;
    seed_agent_revision(
        &store,
        &config,
        target_id,
        &workspace.agents_dir.join(target_id),
    )?;

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let mut client = JsonRpcClient::connect(server_addr).await?;

    // The model passes `"async": "true"` (string) instead of `true` (boolean).
    let payload = serde_json::json!({
        "agent_id": target_id,
        "message": "do the task",
        "async": "true"
    });

    let result = send_tool(
        &mut client,
        "test-spawn-string-bool",
        "agent_spawn",
        payload,
    )
    .await?;

    assert!(
        result.get("error").is_none(),
        "agent.spawn should coerce string 'true' to boolean: {:?}",
        result.get("error")
    );

    Ok(())
}
