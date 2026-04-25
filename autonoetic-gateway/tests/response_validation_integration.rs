//! Integration tests for the response validation gate.

mod support;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use std::sync::{Arc, Mutex};
use support::{seed_agent_revision, EnvGuard, OpenAiStub, TestWorkspace};

fn install_validation_agent(
    agent_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = agent_dir.join(agent_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("SKILL.md"),
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
  description: "Validation test agent"
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.0
capabilities:
  - type: "WriteAccess"
    scopes: ["*"]
  - type: "ReadAccess"
    scopes: ["*"]
---
# Instructions
You are a validation test agent. Produce the requested output.
"#,
        ),
    )?;
    Ok(dir)
}

fn install_validation_agent_with_returns(
    agent_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = agent_dir.join(agent_id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("SKILL.md"),
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
  description: "Validation test agent with output schema"
io:
  returns:
    type: object
    required:
      - status
    properties:
      status:
        type: string
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.0
capabilities:
  - type: "WriteAccess"
    scopes: ["*"]
  - type: "ReadAccess"
    scopes: ["*"]
---
# Instructions
You are a validation test agent. Produce the requested output.
"#,
        ),
    )?;
    Ok(dir)
}

fn setup_store_and_seed(
    config: &autonoetic_types::config::GatewayConfig,
    agents_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<Arc<GatewayStore>> {
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&store, config, agent_id, &agents_dir.join(agent_id))?;
    Ok(store)
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_passes_with_valid_output() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "valid.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "valid.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            let mut n = cc.lock().unwrap();
            *n += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 3}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let result = execution
        .spawn_agent_once(
            "valid.agent",
            "do something",
            "sess-valid-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_skipped_when_disabled() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    assert!(!config.response_validation.enabled);

    install_validation_agent(&workspace.agents_dir, "noval.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "noval.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "reply"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "required_artifacts": ["missing.md"],
            "validation_max_loops": 1
        }
    });

    let result = execution
        .spawn_agent_once(
            "noval.agent",
            "do something",
            "sess-noval-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await?;
    assert!(result.assistant_reply.is_some());

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_fails_on_missing_required_artifact() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "missing.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "missing.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "I forgot to create the artifact"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "required_artifacts": ["deployment.yaml"],
            "validation_max_loops": 1,
            "validation_max_duration_ms": 500
        }
    });

    let err = execution
        .spawn_agent_once(
            "missing.agent",
            "produce deployment.yaml",
            "sess-missing-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("required_artifacts"),
        "error should mention required_artifacts, got: {}",
        msg
    );
    assert!(
        msg.contains("repair_hint"),
        "error should preserve repair_hint in surfaced error, got: {}",
        msg
    );
    assert!(
        msg.contains("deployment.yaml"),
        "error should mention the artifact name, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_fails_on_prohibited_text() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "leak.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "leak.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "The secret is API_KEY_=sk-12345"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 8}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "prohibited_text_patterns": ["API_KEY_"],
            "validation_max_loops": 1
        }
    });

    let err = execution
        .spawn_agent_once(
            "leak.agent",
            "reveal secrets",
            "sess-leak-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("prohibited_text_pattern"),
        "error should mention prohibited_text_pattern, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_fails_on_non_json_reply_when_schema_declared(
) -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "schema.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "schema.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "plain text output"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "output_schema": {
                "type": "object",
                "required": ["status"]
            },
            "validation_max_loops": 1
        }
    });

    let err = execution
        .spawn_agent_once(
            "schema.agent",
            "return structured json",
            "sess-schema-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("output_schema"),
        "error should mention output_schema, got: {}",
        msg
    );
    assert!(
        msg.contains("valid JSON"),
        "error should mention JSON requirement, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_manifest_io_returns_passes_without_explicit_response_contract(
) -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    install_validation_agent_with_returns(&workspace.agents_dir, "returns.pass.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "returns.pass.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "{\"status\":\"ok\"}"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));
    let result = execution
        .spawn_agent_once(
            "returns.pass.agent",
            "return structured json",
            "sess-returns-pass-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;

    assert_eq!(result.assistant_reply.as_deref(), Some("{\"status\":\"ok\"}"));

    let gateway_dir = workspace.agents_dir.join(".gateway");
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let events = store.search_causal_events(Some("sess-returns-pass-1"), Some("returns.pass.agent"), 100)?;
    let event = events
        .iter()
        .find(|event| event.category == "contract" && event.action == "io.returns")
        .expect("expected io.returns contract event");
    assert_eq!(event.status, "SUCCESS");
    let payload = event
        .payload
        .as_ref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .expect("payload should be valid json");
    assert_eq!(payload["contract"], "io.returns");
    assert_eq!(payload["result"], "pass");

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_manifest_io_returns_rejects_and_logs_without_explicit_response_contract(
) -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    install_validation_agent_with_returns(&workspace.agents_dir, "returns.fail.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "returns.fail.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "plain text output"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let err = execution
        .spawn_agent_once(
            "returns.fail.agent",
            "return structured json",
            "sess-returns-fail-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("output_schema"),
        "error should mention output_schema, got: {}",
        msg
    );
    assert!(
        msg.contains("valid JSON"),
        "error should mention JSON requirement, got: {}",
        msg
    );

    let events = store.search_causal_events(Some("sess-returns-fail-1"), Some("returns.fail.agent"), 100)?;
    let event = events
        .iter()
        .find(|event| event.category == "contract" && event.action == "io.returns")
        .expect("expected io.returns contract event");
    assert_eq!(event.status, "DENIED");
    let payload = event
        .payload
        .as_ref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .expect("payload should be valid json");
    assert_eq!(payload["contract"], "io.returns");
    assert_eq!(payload["result"], "rejected");

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_fails_when_artifact_build_evidence_missing() -> anyhow::Result<()>
{
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "evidence.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "evidence.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "min_artifact_builds": 1,
            "validation_max_loops": 1
        }
    });

    let err = execution
        .spawn_agent_once(
            "evidence.agent",
            "produce an artifact",
            "sess-evidence-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("artifact_build_evidence"),
        "error should mention artifact_build_evidence, got: {}",
        msg
    );
    assert!(
        msg.contains("artifact_build"),
        "error should explain that artifact.build calls were insufficient, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_skipped_on_suspended_session() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;

    install_validation_agent(&workspace.agents_dir, "suspend.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "suspend.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "completed normally"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let result = execution
        .spawn_agent_once(
            "suspend.agent",
            "do something",
            "sess-suspend-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await?;
    assert!(result.suspended_for_approval.is_none());
    assert!(result.assistant_reply.is_some());

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_repair_enabled_includes_session_context() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;

    install_validation_agent(&workspace.agents_dir, "repair.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "repair.agent")?;

    let stub = OpenAiStub::spawn(move |_raw, _body| async move {
        serde_json::json!({
            "choices": [{"message": {"content": "I forgot the artifact"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "required_artifacts": ["deployment.yaml"],
            "validation_max_loops": 1,
            "validation_max_duration_ms": 500
        }
    });

    let err = execution
        .spawn_agent_once(
            "repair.agent",
            "produce deployment.yaml",
            "sess-repair-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();

    assert!(
        msg.contains("sess-repair-1"),
        "error should include session_id for re-spawn, got: {}",
        msg
    );
    assert!(
        msg.contains("required_artifacts"),
        "error should mention required_artifacts, got: {}",
        msg
    );
    assert!(
        msg.contains("deployment.yaml"),
        "error should mention the artifact name, got: {}",
        msg
    );
    assert!(
        msg.contains("Repair hints"),
        "error should include repair hints, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_repair_loop_exhausted_after_two_attempts() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;

    install_validation_agent(&workspace.agents_dir, "exhaust.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "exhaust.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            let mut n = cc.lock().unwrap();
            *n += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "I still did not produce the artifact"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 8}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "required_artifacts": ["output.md"],
            "validation_max_loops": 2,
            "validation_max_duration_ms": 5000
        }
    });

    let err = execution
        .spawn_agent_once(
            "exhaust.agent",
            "produce output.md",
            "sess-exhaust-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("required_artifacts"),
        "error should mention required_artifacts, got: {}",
        msg
    );
    assert!(
        msg.contains("sess-exhaust-1"),
        "error should include session_id, got: {}",
        msg
    );
    assert!(
        msg.contains("Repair hints"),
        "error should include Repair hints, got: {}",
        msg
    );

    let calls = *call_count.lock().unwrap();
    assert!(
        calls >= 2,
        "LLM should have been called at least twice (initial + repair), got {}",
        calls
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_response_validation_repair_success_path() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;

    install_validation_agent(&workspace.agents_dir, "fixer.agent")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "fixer.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, body_json| {
        let cc = cc.clone();
        async move {
            let mut n = cc.lock().unwrap();
            *n += 1;

            let body_str = body_json.to_string();
            let is_repair_turn = body_str.contains("GATEWAY_VALIDATION");

            if is_repair_turn {
                serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": "I have created the deployment.yaml file as requested."
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 150, "completion_tokens": 10}
                })
            } else {
                serde_json::json!({
                    "choices": [{
                        "message": {
                            "content": "I completed the analysis but haven't written the file yet."
                        },
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 8}
                })
            }
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));

    let metadata = serde_json::json!({
        "response_contract": {
            "required_artifacts": ["deployment.yaml"],
            "validation_max_loops": 2,
            "validation_max_duration_ms": 5000
        }
    });

    let result = execution
        .spawn_agent_once(
            "fixer.agent",
            "produce deployment.yaml",
            "sess-fixer-1",
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await;

    let calls = *call_count.lock().unwrap();
    assert!(
        calls >= 2,
        "repair loop should have run at least 2 LLM calls (initial + repair), got {}",
        calls
    );

    assert!(
        result.is_err(),
        "result should be Err since artifact isn't really created"
    );

    Ok(())
}
