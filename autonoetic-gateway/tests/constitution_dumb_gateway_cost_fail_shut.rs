//! Constitution Phase 4.8: cost-budget fail-shut when pricing catalog is unavailable.
//!
//! Pins:
//! - price-capped sessions fail before first LLM call when pricing catalog is unavailable
//! - sessions without a price cap continue normally
//! - explicit capability override allows unpriced execution intentionally

mod support;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use std::sync::{Arc, Mutex};
use support::{seed_agent_revision, EnvGuard, OpenAiStub, TestWorkspace};

fn install_budget_agent(
    agent_dir: &std::path::Path,
    agent_id: &str,
    extra_capabilities_yaml: &str,
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
  description: "Cost fail-shut test agent"
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.0
capabilities:
  - type: "WriteAccess"
    scopes: ["*"]
  - type: "ReadAccess"
    scopes: ["*"]
{extra_capabilities_yaml}
---
# Instructions
Return a short answer.
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
async fn catalog_unavailable_with_price_cap_refuses_session_start() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.session_budget.max_session_price_usd = Some(0.01);

    install_budget_agent(&workspace.agents_dir, "cost.fail.agent", "")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "cost.fail.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;

    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");
    let _catalog = EnvGuard::set("AUTONOETIC_OPENROUTER_CATALOG", "0");

    let execution = GatewayExecutionService::new(config, Some(store));
    let err = execution
        .spawn_agent_once(
            "cost.fail.agent",
            "say hello",
            "sess-cost-fail-1",
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
        0,
        "fail-shut must refuse before first LLM call when price cap is set"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("Session start refused"),
        "error should indicate refuse-session-start behavior, got: {}",
        msg
    );
    assert!(
        msg.contains("R-6.5"),
        "error should reference constitutional rule, got: {}",
        msg
    );
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn catalog_unavailable_without_price_cap_starts_normally() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();

    install_budget_agent(&workspace.agents_dir, "cost.nocap.agent", "")?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "cost.nocap.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;

    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");
    let _catalog = EnvGuard::set("AUTONOETIC_OPENROUTER_CATALOG", "0");

    let execution = GatewayExecutionService::new(config, Some(store));
    let result = execution
        .spawn_agent_once(
            "cost.nocap.agent",
            "say hello",
            "sess-cost-nocap-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await?;

    assert_eq!(
        result.assistant_reply.as_deref(),
        Some("ok"),
        "session without price cap should proceed"
    );
    assert_eq!(
        *call_count.lock().unwrap(),
        1,
        "session without price cap should call the LLM normally"
    );
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn override_capability_allows_unpriced_price_capped_session() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.session_budget.max_session_price_usd = Some(0.01);

    install_budget_agent(
        &workspace.agents_dir,
        "cost.override.agent",
        r#"  - type: "budget.no_price_available.allow"
"#,
    )?;
    let store = setup_store_and_seed(&config, &workspace.agents_dir, "cost.override.agent")?;

    let call_count = Arc::new(Mutex::new(0usize));
    let cc = call_count.clone();
    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let cc = cc.clone();
        async move {
            *cc.lock().unwrap() += 1;
            serde_json::json!({
                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
    })
    .await?;

    let _url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", stub.completion_url());
    let _key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");
    let _catalog = EnvGuard::set("AUTONOETIC_OPENROUTER_CATALOG", "0");

    let execution = GatewayExecutionService::new(config, Some(store));
    let result = execution
        .spawn_agent_once(
            "cost.override.agent",
            "say hello",
            "sess-cost-override-1",
            None,
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await?;

    assert_eq!(result.assistant_reply.as_deref(), Some("ok"));
    assert_eq!(
        *call_count.lock().unwrap(),
        1,
        "override capability should allow the LLM call despite unavailable catalog"
    );
    Ok(())
}
