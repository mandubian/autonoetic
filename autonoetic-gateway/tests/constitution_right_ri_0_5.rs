//! Constitution Ri-0.5 — degraded-mode notice before next turn.
//!
//! An agent entering degraded mode must be explicitly notified before the
//! next turn executes, including:
//! - the rule IDs that triggered degraded mode;
//! - evidence payload describing the trigger.

mod support;

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::causal_chain::CausalEventRecord;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use support::manifest_builder::TestManifest;

#[derive(Default)]
struct CaptureSystemPromptDriver {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl CaptureSystemPromptDriver {
    fn first_system_prompt(&self) -> String {
        self.prompts
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("expected at least one LLM call")
    }
}

#[async_trait::async_trait]
impl LlmDriver for CaptureSystemPromptDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let system_prompt = req
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
            .expect("system prompt must be present");
        self.prompts.lock().unwrap().push(system_prompt);

        Ok(CompletionResponse {
            text: "done".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn manifest(agent_id: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..TestManifest::new().build()
    }
}

fn seed_agent_dir(base: &std::path::Path, agent_id: &str) -> std::path::PathBuf {
    // The executor binds the constitution version/digest into the per-turn
    // state attestation tail (P-6.23) whenever a gateway_dir is set; this
    // suite builds executors directly rather than through gateway bootstrap
    // (which normally calls initialize_constitution). Best-effort, idempotent.
    let _ = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    );
    let agent_dir = base.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {}\n", agent_id)).unwrap();
    agent_dir
}

#[tokio::test]
async fn ri_0_5_degraded_mode_notice_injected_with_rule_and_evidence() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "ri05.tester";
    let agent_dir = seed_agent_dir(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let session_id = "session-ri05-notice";
    store
        .create_causal_event(&CausalEventRecord {
            event_id: "evt-ri05-degraded".to_string(),
            agent_id: "gateway".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "session".to_string(),
            action: "session.degraded".to_string(),
            status: "active".to_string(),
            enforced_rules: vec!["P-7.18".to_string()],
            target: None,
            payload: Some(
                serde_json::json!({
                    "source": "operator",
                    "reason": "manual_operator_degrade"
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("manual_operator_degrade".to_string()),
        })
        .unwrap();

    let degraded_sessions = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    degraded_sessions
        .lock()
        .await
        .insert(session_id.to_string());

    let driver = Arc::new(CaptureSystemPromptDriver::default());
    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        driver.clone(),
        agent_dir,
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(session_id)
    .with_degraded_sessions(Some(degraded_sessions));

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let system_prompt = driver.first_system_prompt();
    assert!(
        system_prompt.contains("Degradation Notice (Ri-0.5)"),
        "degraded sessions must inject Ri-0.5 notice"
    );
    assert!(
        system_prompt.contains("Rule IDs: P-7.18"),
        "notice must include rule IDs"
    );
    assert!(
        system_prompt.contains("\"source\":\"operator\"")
            && system_prompt.contains("\"reason\":\"manual_operator_degrade\""),
        "notice must include trigger evidence payload"
    );
}

#[tokio::test]
async fn ri_0_5_normal_session_has_no_degraded_notice() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "ri05.normal";
    let agent_dir = seed_agent_dir(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let driver = Arc::new(CaptureSystemPromptDriver::default());
    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        driver.clone(),
        agent_dir,
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id("session-ri05-normal");

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let system_prompt = driver.first_system_prompt();
    assert!(
        !system_prompt.contains("Degradation Notice (Ri-0.5)"),
        "normal sessions must not inject degraded-mode notice"
    );
}
