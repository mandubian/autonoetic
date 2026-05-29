//! Constitution Ri-0.6 — no silent capability reduction mid-session.
//!
//! Runtime invariant:
//! - turn N+1 capability tiers must not narrow silently;
//! - narrowing is allowed only when degraded mode is active and backed by
//!   `session.degraded` causal evidence;
//! - every detected narrowing emits `session.capability_narrowed`.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::causal_chain::CausalEventRecord;
use autonoetic_types::config::GatewayConfig;
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::tempdir;

fn setup() -> (
    GatewayConfig,
    Arc<GatewayStore>,
    Arc<GatewayExecutionService>,
) {
    let temp = tempdir().unwrap();
    let agents_dir = temp.keep().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let config = GatewayConfig {
        agents_dir,
        ..GatewayConfig::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));
    (config, store, execution)
}

#[derive(Clone)]
struct FixedReplyDriver {
    text: &'static str,
}

#[async_trait::async_trait]
impl LlmDriver for FixedReplyDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: self.text.to_string(),
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
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_agent_dir(agents_dir: &std::path::Path, agent_id: &str) -> std::path::PathBuf {
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {}\n", agent_id)).unwrap();
    agent_dir
}

#[tokio::test]
async fn ri_0_6_rejects_silent_mid_session_narrowing_without_degraded_event() {
    let (config, store, _execution) = setup();
    let session_id = "sess-ri06-silent";
    let agent_id = "ri06.silent";
    let agent_dir = seed_agent_dir(&config.agents_dir, agent_id);

    let degraded_sessions = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        Arc::new(FixedReplyDriver { text: "ok" }),
        agent_dir,
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(config.agents_dir.join(".gateway"))
    .with_session_id(session_id.to_string())
    .with_degraded_sessions(Some(degraded_sessions.clone()));

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Turn one".to_string()),
    ];
    let first = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(first, TurnOutcome::Completed(_)));

    // Simulate an unauthorized mid-session narrowing: degraded set flips without
    // the required `session.degraded` causal event.
    degraded_sessions
        .lock()
        .await
        .insert(session_id.to_string());
    history.push(Message::user("Turn two".to_string()));

    let err = runtime
        .execute_with_history(&mut history)
        .await
        .expect_err("silent narrowing must fail shut");
    let msg = err.to_string();
    assert!(
        msg.contains("Ri-0.6 violation"),
        "expected Ri-0.6 failure, got: {}",
        msg
    );
    assert!(
        msg.contains("session.degraded"),
        "error should require degraded causal evidence, got: {}",
        msg
    );
}

#[tokio::test]
async fn ri_0_6_operator_degrade_allows_narrowing_and_records_event() {
    let (config, store, execution) = setup();
    let session_id = "sess-ri06-operator";
    let agent_id = "ri06.operator";
    let agent_dir = seed_agent_dir(&config.agents_dir, agent_id);

    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        Arc::new(FixedReplyDriver { text: "ok" }),
        agent_dir,
        default_registry(),
        Some(store.clone()),
    )
    .with_gateway_dir(config.agents_dir.join(".gateway"))
    .with_session_id(session_id.to_string())
    .with_degraded_sessions(Some(execution.degraded_sessions()));

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Turn one".to_string()),
    ];
    let first = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(first, TurnOutcome::Completed(_)));

    execution
        .degrade_session_with_options(session_id, "operator_degrade", false)
        .await
        .unwrap();

    history.push(Message::user("Turn two".to_string()));
    let second = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(second, TurnOutcome::Completed(_)));

    let events = store
        .search_causal_events(Some(session_id), None, 128)
        .unwrap();
    let narrowed = events
        .iter()
        .find(|e| e.action == "session.capability_narrowed")
        .expect("capability narrowing event must be recorded");
    assert!(
        narrowed.enforced_rules.contains(&"Ri-0.6".to_string()),
        "narrowing event must cite Ri-0.6"
    );
    let payload: serde_json::Value =
        serde_json::from_str(narrowed.payload.as_deref().expect("payload")).unwrap();
    assert_eq!(payload["narrowing_path"], "operator_command");
    assert_eq!(
        payload["previous_allowed_tiers"],
        serde_json::json!(["core", "specialized", "workflow"])
    );
    assert_eq!(
        payload["current_allowed_tiers"],
        serde_json::json!(["core"])
    );
}

#[tokio::test]
async fn ri_0_6_rule_driven_degrade_path_is_accepted() {
    let (config, store, _execution) = setup();
    let session_id = "sess-ri06-rule";
    let agent_id = "ri06.rule";
    let agent_dir = seed_agent_dir(&config.agents_dir, agent_id);

    let degraded_sessions = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        Arc::new(FixedReplyDriver { text: "ok" }),
        agent_dir,
        default_registry(),
        Some(store.clone()),
    )
    .with_gateway_dir(config.agents_dir.join(".gateway"))
    .with_session_id(session_id.to_string())
    .with_degraded_sessions(Some(degraded_sessions.clone()));

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Turn one".to_string()),
    ];
    let first = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(first, TurnOutcome::Completed(_)));

    store
        .create_causal_event(&CausalEventRecord {
            event_id: "evt-ri06-rule".to_string(),
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
                    "reason": "loop_guard_sub_trip_warning"
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("loop_guard_sub_trip_warning".to_string()),
        })
        .unwrap();
    degraded_sessions
        .lock()
        .await
        .insert(session_id.to_string());

    history.push(Message::user("Turn two".to_string()));
    let second = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(second, TurnOutcome::Completed(_)));

    let events = store
        .search_causal_events(Some(session_id), None, 128)
        .unwrap();
    let narrowed = events
        .iter()
        .find(|e| e.action == "session.capability_narrowed")
        .expect("capability narrowing event");
    let payload: serde_json::Value =
        serde_json::from_str(narrowed.payload.as_deref().expect("payload")).unwrap();
    assert_eq!(payload["narrowing_path"], "degraded_mode");
}
