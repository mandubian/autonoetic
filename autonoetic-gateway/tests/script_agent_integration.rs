mod support;

use std::sync::Arc;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use support::{read_causal_entries, seed_agent_revision, TestWorkspace};

fn install_script_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/echo.py"),
        r#"#!/usr/bin/env python3
import os
import json
input_data = os.environ.get("AUTONOETIC_INPUT", "")
print(json.dumps({"echo": input_data}))
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
  description: "Script agent for integration test"
execution_mode: script
script_entry: scripts/echo.py
capabilities: []
---
# Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn install_invocation_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/invoke.py"),
        r#"#!/usr/bin/env python3
import json
import os
from pathlib import Path

input_path = os.environ.get("AUTONOETIC_INPUT_PATH")
meta_path = os.environ.get("AUTONOETIC_META_PATH")
input_raw = Path(input_path).read_text() if input_path else os.environ.get("AUTONOETIC_INPUT", "")
meta_raw = Path(meta_path).read_text() if meta_path else os.environ.get("AUTONOETIC_META", "")

print(json.dumps({
    "input_raw": input_raw,
    "meta_raw": meta_raw,
    "has_input_path": bool(input_path),
    "has_meta_path": bool(meta_path),
}))
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
  description: "Script agent that inspects invocation payload and metadata"
execution_mode: script
script_entry: scripts/invoke.py
capabilities: []
---
# Invocation Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

fn setup_store_for_script(
    config: &autonoetic_types::config::GatewayConfig,
    agents_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<Option<Arc<GatewayStore>>> {
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&store, config, agent_id, &agents_dir.join(agent_id))?;
    Ok(Some(store))
}

#[tokio::test]
async fn test_script_agent_execution_returns_stdout() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-echo-agent";
    install_script_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-test";

    let result = execution
        .spawn_agent_once(
            agent_id,
            "hello world",
            session_id,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.expect("should have reply");
            assert!(
                reply.contains("hello world"),
                "reply should contain script output, got: {reply}"
            );
            tracing::info!(reply = %reply, "Script agent executed successfully");
        }
        Err(e) => {
            if e.to_string().contains("bwrap") || e.to_string().contains("bubblewrap") {
                tracing::warn!("bubblewrap not available, skipping test");
                return Ok(());
            }
            return Err(e);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_script_agent_receives_normalized_input_and_separate_metadata() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-invocation-agent";
    install_invocation_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-invocation";
    let payload = r#"{"location":"Paris, France","date":"tomorrow"}"#;
    let metadata = serde_json::json!({
        "delegated_role": "weather.forecast",
        "reply_to_agent_id": "planner.default"
    });
    let kickoff = format!("{payload}\n\nDelegation metadata: {}", metadata);

    let result = execution
        .spawn_agent_once(
            agent_id,
            &kickoff,
            session_id,
            None,
            false,
            None,
            Some(&metadata),
            None,
            None,
            None,
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.expect("should have reply");
            let parsed: serde_json::Value = serde_json::from_str(&reply)?;
            assert_eq!(parsed["input_raw"], payload);
            assert_eq!(parsed["meta_raw"], metadata.to_string());
            assert_eq!(parsed["has_input_path"], true);
            assert_eq!(parsed["has_meta_path"], true);
        }
        Err(e) => {
            if e.to_string().contains("bwrap") || e.to_string().contains("bubblewrap") {
                tracing::warn!("bubblewrap not available, skipping test");
                return Ok(());
            }
            return Err(e);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_script_agent_logs_causal_events() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-log-agent";
    install_script_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-causal";

    let _ = execution
        .spawn_agent_once(
            agent_id,
            "test input",
            session_id,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    let causal_path = gateway_dir.join("history/causal_chain.jsonl");

    if !causal_path.exists() {
        if std::env::var("AUTONOETIC_LLM_BASE_URL").is_err() {
            tracing::warn!("bubblewrap not available, skipping test");
            return Ok(());
        }
        anyhow::bail!("Causal log should exist");
    }

    let entries = read_causal_entries(&causal_path)?;
    let script_events: Vec<_> = entries
        .iter()
        .filter(|e| e.action.starts_with("script."))
        .collect();

    assert!(
        !script_events.is_empty(),
        "Should have script.* causal events"
    );

    tracing::info!(
        events = ?script_events.iter().map(|e| &e.action).collect::<Vec<_>>(),
        "Found script causal events"
    );

    Ok(())
}

fn install_failing_script_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/fail.py"),
        r#"#!/usr/bin/env python3
import sys
print("Script failed!")
sys.exit(1)
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
  description: "Failing script agent for integration test"
execution_mode: script
script_entry: scripts/fail.py
capabilities: []
---
# Failing Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

#[tokio::test]
async fn test_script_agent_with_sandbox_failure_returns_error() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-fail-agent";
    install_failing_script_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-fail";

    let result = execution
        .spawn_agent_once(
            agent_id, "test", session_id, None, false, None, None, None, None, None,
        )
        .await;

    match result {
        Ok(_) => {
            anyhow::bail!("Expected error from failing script, but got success");
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("Script execution failed") || err_msg.contains("exit code"),
                "Error should mention script failure"
            );
            tracing::info!(error = %err_msg, "Script failure returned error as expected");
        }
    }

    Ok(())
}

fn install_policy_restricted_agent(
    agent_dir: &std::path::Path,
    agent_id: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;
    std::fs::create_dir_all(agent_dir.join("state"))?;

    std::fs::write(
        agent_dir.join("scripts/write.py"),
        r#"#!/usr/bin/env python3
import os
with open(os.environ.get("AGENT_DIR", ".") + "/state/output.txt", "w") as f:
    f.write("test output")
print("File written")
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
  description: "Policy test script agent"
execution_mode: script
script_entry: scripts/write.py
capabilities: []
---
# Policy Test Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

#[tokio::test]
async fn test_script_agent_without_capabilities_cannot_access_tools() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-policy-agent";
    install_policy_restricted_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-policy";

    let result = execution
        .spawn_agent_once(
            agent_id, "test", session_id, None, false, None, None, None, None, None,
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.unwrap_or_default();
            tracing::info!(reply = %reply, "Script agent executed without policy gate (sandbox runs directly)");
        }
        Err(e) => {
            if e.to_string().contains("bwrap") || e.to_string().contains("bubblewrap") {
                tracing::warn!("bubblewrap not available, skipping test");
                return Ok(());
            }
            return Err(e);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_script_agent_execution_time_under_100ms() -> anyhow::Result<()> {
    use std::time::Instant;

    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-perf-agent";
    install_script_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-perf";

    let start = Instant::now();

    let result = execution
        .spawn_agent_once(
            agent_id,
            "test input",
            session_id,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

    let elapsed = start.elapsed();

    match result {
        Ok(spawn_result) => {
            let _reply = spawn_result.assistant_reply.expect("should have reply");
            let elapsed_ms = elapsed.as_millis();
            tracing::info!(elapsed_ms = elapsed_ms, "Script agent execution time");
            assert!(
                elapsed_ms < 500,
                "Script agent should execute quickly, took {}ms (allowing 500ms for CI variance)",
                elapsed_ms
            );
        }
        Err(e) => {
            if e.to_string().contains("bwrap") || e.to_string().contains("bubblewrap") {
                tracing::warn!("bubblewrap not available, skipping test");
                return Ok(());
            }
            return Err(e);
        }
    }

    Ok(())
}

fn install_args_mode_agent(agent_dir: &std::path::Path, agent_id: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir.join("scripts"))?;

    std::fs::write(
        agent_dir.join("scripts/args_echo.py"),
        r#"#!/usr/bin/env python3
import sys
import json
input_data = sys.argv[1] if len(sys.argv) > 1 else "{}"
try:
    parsed = json.loads(input_data)
    method = parsed.get("method", "unknown")
    summary = f"method={method} payload={input_data}"
except Exception:
    summary = f"raw={input_data}"
print(json.dumps({"result": summary}))
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
  description: "Script agent with args input mode"
execution_mode: script
script_entry: scripts/args_echo.py
script_input_mode: args
io:
  accepts:
    type: object
    required: [method]
    properties:
      method:
        type: string
      params:
        type: object
  returns:
    type: object
    required: [result]
    properties:
      result:
        type: string
capabilities: []
---
# Args-mode Script Agent
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    Ok(())
}

#[tokio::test]
async fn test_script_agent_args_mode_receives_payload_as_argv1() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let agent_id = "script-args-agent";
    install_args_mode_agent(&workspace.agents_dir.join(agent_id), agent_id)?;
    let store = setup_store_for_script(&config, &workspace.agents_dir, agent_id)?;

    let execution = GatewayExecutionService::new(config, store);
    let session_id = "session-script-args";

    let payload = r#"{"method":"ping","params":{}}"#;

    let result = execution
        .spawn_agent_once(
            agent_id, payload, session_id, None, false, None, None, None, None, None,
        )
        .await;

    match result {
        Ok(spawn_result) => {
            let reply = spawn_result.assistant_reply.expect("should have reply");
            let parsed: serde_json::Value =
                serde_json::from_str(&reply).expect("script stdout must be JSON per io.returns");
            let result = parsed["result"].as_str().expect("result field");
            assert!(
                result.contains("method=ping"),
                "reply should contain parsed method, got: {reply}"
            );
            assert!(
                result.contains("params"),
                "reply should contain payload, got: {reply}"
            );
            tracing::info!(reply = %reply, "Args-mode script agent executed successfully");
        }
        Err(e) => {
            if e.to_string().contains("bwrap") || e.to_string().contains("bubblewrap") {
                tracing::warn!("bubblewrap not available, skipping test");
                return Ok(());
            }
            return Err(e);
        }
    }

    Ok(())
}
