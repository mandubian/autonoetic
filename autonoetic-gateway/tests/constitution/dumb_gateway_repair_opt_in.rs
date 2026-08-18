//! Constitution Phase 4.1: gateway repair loop is opt-in per manifest.
//!
//! Pins three invariants:
//! - repair is disabled by default even when gateway repair subsystem is on
//! - opt-in via `io.output_policy.repair.auto: true` enables bounded repair
//! - system ceiling caps agent-declared max attempts


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use std::sync::{Arc, Mutex};
use crate::support::{seed_agent_revision, EnvGuard, OpenAiStub, TestWorkspace};

fn install_validation_agent(
    agent_dir: &std::path::Path,
    agent_id: &str,
    output_policy_block: &str,
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
  description: "Repair opt-in test agent"
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.0
capabilities:
  - type: "WriteAccess"
    scopes: ["*"]
  - type: "ReadAccess"
    scopes: ["*"]
{output_policy_block}
---
# Instructions
Return normal text output.
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
async fn repair_disabled_by_default_without_manifest_opt_in() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;
    config.response_validation.max_repair_attempts_ceiling = 2;

    install_validation_agent(
        &workspace.agents_dir,
        "repair.default_off.agent",
        r#"io:
  output_policy:
    required_artifacts: ["required.md"]
"#,
    )?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "repair.default_off.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "artifact missing"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));
    let _ = execution
        .spawn_agent_once(
            "repair.default_off.agent",
            "produce required.md",
            "sess-repair-default-off",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await
        .unwrap_err();

    assert_eq!(
        *call_count.lock().unwrap(),
        1,
        "without io.output_policy.repair.auto opt-in, gateway must not run repair turns"
    );
    Ok(())
}

#[serial_test::serial]
#[test]
fn repair_opt_in_runs_bounded_repair_turn() -> anyhow::Result<()> {
    // #1090: see repair_attempts_are_capped_by_system_ceiling.
    crate::support::run_with_big_stack(repair_opt_in_runs_bounded_repair_turn_body)
}

async fn repair_opt_in_runs_bounded_repair_turn_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;
    config.response_validation.max_repair_attempts_ceiling = 2;

    install_validation_agent(
        &workspace.agents_dir,
        "repair.opt_in.agent",
        r#"io:
  output_policy:
    required_artifacts: ["required.md"]
    repair:
      auto: true
      max_attempts: 1
    validation_max_duration_ms: 5000
"#,
    )?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "repair.opt_in.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "still missing artifact"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));
    let _ = execution
        .spawn_agent_once(
            "repair.opt_in.agent",
            "produce required.md",
            "sess-repair-opt-in",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await
        .unwrap_err();

    assert!(
        *call_count.lock().unwrap() >= 2,
        "with opt-in enabled, gateway should run at least one repair turn"
    );
    Ok(())
}

#[serial_test::serial]
#[test]
fn repair_attempts_are_capped_by_system_ceiling() -> anyhow::Result<()> {
    // #1090: the LLM roundtrip chain overflows the default 2 MiB
    // `#[tokio::test]` stack in debug builds; run on the big-stack runtime.
    crate::support::run_with_big_stack(repair_attempts_are_capped_by_system_ceiling_body)
}

async fn repair_attempts_are_capped_by_system_ceiling_body() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.response_validation.enabled = true;
    config.response_validation.repair_enabled = true;
    config.response_validation.max_repair_attempts_ceiling = 1;

    install_validation_agent(
        &workspace.agents_dir,
        "repair.ceiling.agent",
        r#"io:
  output_policy:
    required_artifacts: ["required.md"]
    repair:
      auto: true
      max_attempts: 5
    validation_max_duration_ms: 5000
"#,
    )?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "repair.ceiling.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "still invalid"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;
    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));
    let _ = execution
        .spawn_agent_once(
            "repair.ceiling.agent",
            "produce required.md",
            "sess-repair-ceiling",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await
        .unwrap_err();

    assert_eq!(
        *call_count.lock().unwrap(),
        2,
        "system ceiling=1 means exactly initial call + one repair attempt"
    );
    Ok(())
}
