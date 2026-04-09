mod support;

use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::promotion::PromotionRole;
use support::{seed_agent_revision, EnvGuard, TestWorkspace};

const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

fn install_deterministic_reply_agent(
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
  description: "Deterministic promotion contract test agent"
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
middleware:
  pre_process: "python3 skip_hook.py"
---
# Deterministic Reply Agent
Always return deterministic output.
"#,
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
    Ok(())
}

#[tokio::test]
async fn test_required_promotion_record_fails_when_missing() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let _api_key = EnvGuard::set(OPENAI_API_KEY_ENV, "test-key");
    let agent_id = "evaluator.default";
    let agent_dir = workspace.agents_dir.join(agent_id);
    install_deterministic_reply_agent(&agent_dir, agent_id)?;

    let config = workspace.gateway_config();
    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&store, &config, agent_id, &agent_dir)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    let err = execution
        .spawn_agent_once(
            agent_id,
            "Validate artifact and summarize",
            "session-promotion-contract-missing",
            None,
            false,
            None,
            Some(&serde_json::json!({
                "require_promotion_record": true,
                "promotion_artifact_id": "art_contract_missing",
                "promotion_role": "evaluator"
            })),
            None,
            None,
        )
        .await
        .expect_err("spawn should fail when required promotion record is missing");

    assert!(
        err.to_string()
            .contains("completed without a matching promotion.record within"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn test_required_promotion_record_succeeds_when_present() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let _api_key = EnvGuard::set(OPENAI_API_KEY_ENV, "test-key");
    let agent_id = "evaluator.default";
    let artifact_id = "art_contract_present";
    let agent_dir = workspace.agents_dir.join(agent_id);
    install_deterministic_reply_agent(&agent_dir, agent_id)?;

    let gateway_dir = workspace.agents_dir.join(".gateway");
    let store = PromotionStore::new(&gateway_dir)?;
    store.record_promotion(
        artifact_id.to_string(),
        None,
        None,
        PromotionRole::Evaluator,
        agent_id,
        true,
        vec![],
        Some("pre-recorded evaluator pass".to_string()),
    )?;

    let config = workspace.gateway_config();
    let gw_store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_agent_revision(&gw_store, &config, agent_id, &agent_dir)?;

    let execution = GatewayExecutionService::new(config, Some(gw_store));
    let result = execution
        .spawn_agent_once(
            agent_id,
            "Validate artifact and summarize",
            "session-promotion-contract-present",
            None,
            false,
            None,
            Some(&serde_json::json!({
                "require_promotion_record": true,
                "promotion_artifact_id": artifact_id,
                "promotion_role": "evaluator"
            })),
            None,
            None,
        )
        .await?;

    assert_eq!(
        result.assistant_reply.as_deref(),
        Some("deterministic reply")
    );

    Ok(())
}
