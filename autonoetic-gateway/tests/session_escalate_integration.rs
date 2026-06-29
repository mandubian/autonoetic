//! Integration tests: session.escalate tool and human escalation approval flow.
//!
//! Verifies the full escalation lifecycle:
//!   1. Agent calls `session.escalate(target="human")` — creates approval request, returns `escalation_required: true`.
//!   2. Lifecycle detects sentinel, saves checkpoint with `YieldReason::HumanEscalation`, returns `TurnOutcome::Escalated`.
//!   3. Operator approves the escalation via `approve_request`.
//!   4. Session resumes from checkpoint with operator guidance injected as system message.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::{approve_request, load_approval_requests};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::ScheduledAction;
use std::sync::{Arc, Mutex};
use support::{EnvGuard, OpenAiStub};

const LLM_BASE_URL_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_ENV: &str = "AUTONOETIC_LLM_API_KEY";

fn make_escalation_stub_responses() -> Vec<serde_json::Value> {
    let tool_call_response = serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "tc-escalate-001",
                    "type": "function",
                    "function": {
                        "name": "session_escalate",
                        "arguments": serde_json::json!({
                            "reason": "I am stuck on a critical bug",
                            "context": "Tried multiple approaches but none work",
                            "target": "human",
                            "urgency": "high",
                            "suggested_actions": ["Try debugging with print statements"]
                        }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    });

    let final_text_response = serde_json::json!({
        "choices": [{
            "message": {
                "content": "Thank you for the guidance. I will proceed with the debugger approach."
            },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 20, "completion_tokens": 8 }
    });

    vec![tool_call_response, final_text_response]
}

fn test_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "escalation-test-agent".to_string(),
            name: "Escalation Test Agent".to_string(),
            description: "Test agent for escalation flow".to_string(),
            singleton: false,
        },
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        capabilities: vec![],
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        allowed_tool_tiers: vec![],
        execution_mode: autonoetic_types::agent::ExecutionMode::Reasoning,
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_test_agent(
    config: &autonoetic_types::config::GatewayConfig,
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
) -> anyhow::Result<()> {
    let agent_dir = config.agents_dir.join("escalation-test-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
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
  id: "escalation-test-agent"
  name: "Escalation Test Agent"
  description: "Test agent for escalation flow"
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
  temperature: 0.0
---

You are a test agent for escalation flow.
"#,
    )?;
    support::seed_agent_revision(store, config, "escalation-test-agent", &agent_dir)?;
    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_session_escalate_creates_approval_and_suspends() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let session_id = "escalation-test-session";
    let args = serde_json::json!({
        "reason": "I am stuck on a critical bug",
        "context": "Tried multiple approaches but none work",
        "target": "human",
        "urgency": "high",
        "suggested_actions": ["Try debugging with print statements"],
    });

    let agent_dir = workspace.agents_dir.join("escalation-test-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;

    let result = registry.execute(
        "session_escalate",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some(session_id),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_ok(), "session.escalate should succeed");
    let response: serde_json::Value = serde_json::from_str(&result.unwrap())?;

    assert_eq!(
        response.get("escalation_required"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(response.get("request_id").is_some());
    assert_eq!(
        response.get("escalation_type"),
        Some(&serde_json::json!("human"))
    );

    let request_id = response["request_id"].as_str().unwrap();

    // Verify approval was created in the store
    let pending = load_approval_requests(&config, Some(store.as_ref()))?;
    let escalation_approval = pending.iter().find(|r| r.request_id == request_id);
    assert!(
        escalation_approval.is_some(),
        "escalation approval should exist"
    );

    let approval = escalation_approval.unwrap();
    assert!(
        matches!(
            &approval.action,
            ScheduledAction::SessionEscalate { reason, urgency, .. }
                if reason == "I am stuck on a critical bug" && urgency == "high"
        ),
        "approval action should be SessionEscalate with correct fields, got: {:?}",
        approval.action
    );
    assert_eq!(approval.session_id, session_id);

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_session_escalate_non_human_no_approval() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let session_id = "escalation-test-session-2";
    let args = serde_json::json!({
        "reason": "Need advice",
        "context": "General question",
        "target": "reasoning_llm",
    });

    let agent_dir = workspace.agents_dir.join("escalation-test-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;

    let result = registry.execute(
        "session_escalate",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some(session_id),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_ok());
    let response: serde_json::Value = serde_json::from_str(&result.unwrap())?;

    assert_eq!(response.get("escalation_required"), None);
    assert_eq!(
        response.get("escalation_type"),
        Some(&serde_json::json!("reasoning_llm"))
    );

    // No approval should be created for reasoning_llm
    let pending = load_approval_requests(&config, Some(store.as_ref()))?;
    assert!(
        pending.is_empty(),
        "reasoning_llm escalation should not create approval"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_session_escalate_specialist_no_approval() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let session_id = "escalation-test-session-3";
    let args = serde_json::json!({
        "reason": "Need technical help",
        "context": "Architecture question",
        "target": "specialist",
    });

    let agent_dir = workspace.agents_dir.join("escalation-test-agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;

    let result = registry.execute(
        "session_escalate",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &args.to_string(),
        Some(session_id),
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(result.is_ok());
    let response: serde_json::Value = serde_json::from_str(&result.unwrap())?;

    assert_eq!(response.get("escalation_required"), None);
    assert_eq!(
        response.get("escalation_type"),
        Some(&serde_json::json!("specialist"))
    );

    let pending = load_approval_requests(&config, Some(store.as_ref()))?;
    assert!(
        pending.is_empty(),
        "specialist escalation should not create approval"
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn test_escalation_approval_resume_injects_guidance() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.approval_dwell_multiplier = 0.0;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);

    seed_test_agent(&config, store.as_ref())?;

    let responses = Arc::new(Mutex::new(make_escalation_stub_responses()));
    let responses_clone = Arc::clone(&responses);

    let stub = OpenAiStub::spawn(move |_raw, _body| {
        let responses = Arc::clone(&responses_clone);
        async move {
            let mut queue = responses.lock().unwrap();
            if queue.is_empty() {
                serde_json::json!({
                    "choices": [{"message": {"content": "unexpected extra call"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })
            } else {
                queue.remove(0)
            }
        }
    })
    .await?;

    let _base_url = EnvGuard::set(LLM_BASE_URL_ENV, stub.completion_url());
    let _api_key = EnvGuard::set(LLM_API_KEY_ENV, "test-key");

    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let session_id = "escalation-resume-session";
    let task_id = "escalation-resume-task";

    // First spawn — agent should escalate to human
    let first_result = execution
        .spawn_agent_once(
            "escalation-test-agent",
            "I need help with this problem. session.escalate(target='human', reason='stuck', context='tried everything')",
            session_id,
            None,
            false,
            None,
            None,
            None,
            Some(task_id),
            None,
        &[],
        )
        .await?;
    assert!(
        first_result.suspended_for_user_input,
        "first run should suspend for escalation/user input"
    );

    // The session should have a checkpoint with HumanEscalation
    let checkpoint = load_latest_checkpoint(&config, session_id)?;
    assert!(
        checkpoint.is_some(),
        "checkpoint should be saved for escalation"
    );
    let cp = checkpoint.unwrap();
    assert!(
        matches!(&cp.yield_reason, YieldReason::HumanEscalation { .. }),
        "yield reason should be HumanEscalation, got {:?}",
        cp.yield_reason
    );

    // Find the escalation approval
    let pending = load_approval_requests(&config, Some(store.as_ref()))?;
    let escalation_approval = pending
        .iter()
        .find(|r| matches!(&r.action, ScheduledAction::SessionEscalate { .. }));
    assert!(
        escalation_approval.is_some(),
        "escalation approval should exist"
    );
    let approval = escalation_approval.unwrap();
    let request_id = approval.request_id.clone();

    // Approve the escalation with guidance
    approve_request(
        &config,
        Some(store.as_ref()),
        &request_id,
        "test-operator",
        Some("Try using a debugger to step through the code".to_string()),
        None,
        None,
        None,
    )?;

    // Verify the approval was recorded
    let updated = store.get_approval(&request_id)?;
    assert!(
        updated.is_some(),
        "approved escalation should still be in store"
    );
    let updated = updated.unwrap();
    assert!(
        updated.status.is_some(),
        "approval status should be set after decision"
    );

    // Verify decision_reason is persisted (the operator's guidance note)
    assert_eq!(
        updated.decision_reason.as_deref(),
        Some("Try using a debugger to step through the code"),
        "decision_reason should carry operator guidance, not the agent's original reason"
    );

    // The original `reason` field should still be the agent's escalation justification
    assert!(
        updated.reason.is_some(),
        "reason should remain the agent's original escalation reason"
    );

    // Second spawn — should resume from escalation checkpoint with guidance injected
    let second_result = execution
        .spawn_agent_once(
            "escalation-test-agent",
            "Continue with the guidance",
            session_id,
            None,
            false,
            None,
            None,
            None,
            Some(task_id),
            None,
        &[],
        )
        .await?;

    // The session should have resumed (not suspended again)
    assert!(
        second_result.suspended_for_approval.is_none(),
        "resumed session should not suspend for approval"
    );
    assert!(
        !second_result.suspended_for_user_input,
        "resumed session should complete without suspension"
    );

    Ok(())
}
