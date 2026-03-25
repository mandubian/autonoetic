mod support;

use autonoetic_gateway::GatewayExecutionService;
use support::{EnvGuard, OpenAiStub, TestWorkspace};

const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";

fn install_schema_validation_agent(
    agent_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    std::fs::write(
        agent_dir.join("skip_hook.py"),
        r#"
import json
print(json.dumps({"skip_llm": True, "assistant_reply": "deterministic reply"}))
"#,
    )?;

    let skill_md = format!(
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
  description: "Schema validation integration test agent"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
io:
  accepts:
    type: object
    required:
      - query
    properties:
      query:
        type: string
middleware:
  pre_process: "python3 skip_hook.py"
---
# Schema Validation Agent
Always return deterministic output.
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    Ok(())
}

#[tokio::test]
async fn test_spawn_runs_for_plain_text_and_schema_matching_json_inputs() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let target_agent_id = "schema-test";
    install_schema_validation_agent(&workspace.agents_dir.join(target_agent_id), target_agent_id)?;

    let stub = OpenAiStub::spawn(|_, _| async move {
        serde_json::json!({
            "choices": [{
                "message": { "content": "stub assistant reply" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1
            }
        })
    })
    .await?;
    let _base_url = EnvGuard::set(LLM_BASE_URL_OVERRIDE_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_OVERRIDE_ENV, "test-key");

    let execution = GatewayExecutionService::new(workspace.gateway_config(), None);
    let mismatched_session_id = "session-schema-mismatch";
    let valid_session_id = "session-schema-valid";

    let result = execution
        .spawn_agent_once(
            target_agent_id,
            "plain text input that does not match object schema",
            mismatched_session_id,
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .await?;

    assert_eq!(result.session_id, mismatched_session_id);
    assert_eq!(
        result.assistant_reply.as_deref(),
        Some("deterministic reply")
    );

    let result = execution
        .spawn_agent_once(
            target_agent_id,
            r#"{"query":"what is the weather"}"#,
            valid_session_id,
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .await?;

    assert_eq!(result.session_id, valid_session_id);
    assert_eq!(
        result.assistant_reply.as_deref(),
        Some("deterministic reply")
    );

    // Schema validation outcomes are no longer mirrored to `.gateway/history/causal_chain.jsonl`
    // (gateway causal file logging is deprecated in favor of gateway.db).
    Ok(())
}
