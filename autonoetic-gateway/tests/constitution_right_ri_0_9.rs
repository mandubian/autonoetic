//! Constitution Ri-0.9 — last-word opportunity before degrade / emergency stop.
//!
//! "Where practical" must be explicit on stop paths:
//! - when true: gateway queues a last-word notice and records a causal event (`session.last_word_notice`);
//! - when false: gateway records that the opportunity was foreclosed (`session.last_word_foreclosed`).
//! When a turn completes after notice delivery, lifecycle records `session.last_word_response`.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState};
use autonoetic_types::config::GatewayConfig;
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

fn write_min_checkpoint(config: &GatewayConfig, session_id: &str) {
    let cp = SessionCheckpoint {
        history: vec![
            Message::system("system".to_string()),
            Message::user("hello".to_string()),
        ],
        turn_counter: 1,
        loop_guard_state: LoopGuard {
            max_loops_without_progress: 5,
            max_tool_failures: 5,
            max_consecutive_same_progress: 1,
            max_child_failures: 3,
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            ..Default::default()
        },
        session_state: SessionState::Normal,
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        agent_id: "planner.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-000001".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: chrono::Utc::now().to_rfc3339(),
        pending_tool_state: None,
        llm_rounds_consumed: 0,
        tool_invocations_consumed: 0,
        tokens_consumed: 0,
        estimated_cost_usd: 0.0,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
            feedback_events: vec![],
    };
    save_checkpoint(config, &cp).unwrap();
}

#[tokio::test]
async fn ri_0_9_degrade_notifies_where_practical() {
    let (_config, store, execution) = setup();
    let session_id = "sess-ri09-degrade";

    let result = execution
        .degrade_session_with_options(session_id, "operator_degrade", true)
        .await
        .unwrap();

    assert_eq!(result["ok"], true);
    assert_eq!(result["notify_where_practical"], true);

    let pending_messages = store.fetch_undelivered_messages(session_id).unwrap();
    assert_eq!(
        pending_messages.len(),
        1,
        "Ri-0.9 notice should enqueue one last-word message"
    );
    assert!(
        pending_messages[0]
            .message
            .contains("Gateway Notice Ri-0.9"),
        "queued message should be an Ri-0.9 notice"
    );

    let events = store
        .search_causal_events(Some(session_id), None, 64)
        .unwrap();
    let notice = events
        .iter()
        .find(|e| e.action == "session.last_word_notice")
        .expect("must record session.last_word_notice");
    assert!(
        notice.enforced_rules.contains(&"Ri-0.9".to_string()),
        "Ri-0.9 event must carry right ID"
    );
    let payload = notice.payload.as_ref().expect("notice payload");
    assert!(payload.contains("\"where_practical\":true"));
    assert!(payload.contains("\"trigger\":\"degrade\""));
}

#[tokio::test]
async fn ri_0_9_degrade_can_foreclose_when_not_practical() {
    let (_config, store, execution) = setup();
    let session_id = "sess-ri09-foreclose";

    let result = execution
        .degrade_session_with_options(session_id, "auto_degrade", false)
        .await
        .unwrap();
    assert_eq!(result["notify_where_practical"], false);

    let pending_messages = store.fetch_undelivered_messages(session_id).unwrap();
    assert!(
        pending_messages.is_empty(),
        "foreclosed path must not enqueue a last-word notice"
    );

    let events = store
        .search_causal_events(Some(session_id), None, 64)
        .unwrap();
    let foreclosed = events
        .iter()
        .find(|e| e.action == "session.last_word_foreclosed")
        .expect("must record session.last_word_foreclosed");
    let payload = foreclosed.payload.as_ref().expect("foreclosed payload");
    assert!(payload.contains("\"where_practical\":false"));
    assert!(payload.contains("\"trigger\":\"degrade\""));
}

#[tokio::test]
async fn ri_0_9_emergency_stop_explicit_where_practical_flag() {
    let (config, store, execution) = setup();
    let root_session_id = "root-ri09-estop";
    write_min_checkpoint(&config, root_session_id);

    let result = execution
        .emergency_stop_root_session_with_options(
            root_session_id,
            "operator_test_stop",
            "operator",
            "alice",
            "manual",
            None,
            true,
        )
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["notify_where_practical"], true);

    let pending_messages = store.fetch_undelivered_messages(root_session_id).unwrap();
    assert_eq!(
        pending_messages.len(),
        1,
        "where_practical=true should enqueue a last-word notice before stop"
    );
    assert!(
        pending_messages[0]
            .message
            .contains("Trigger: emergency_stop"),
        "message should identify emergency_stop trigger"
    );

    let events = store
        .search_causal_events(Some(root_session_id), None, 128)
        .unwrap();
    let notice = events
        .iter()
        .find(|e| e.action == "session.last_word_notice")
        .expect("must record last-word notice for emergency_stop");
    let payload = notice.payload.as_ref().expect("notice payload");
    assert!(payload.contains("\"where_practical\":true"));
    assert!(payload.contains("\"trigger\":\"emergency_stop\""));
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

fn manifest_simple(agent_id: &str) -> AgentManifest {
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
            singleton: false,
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_agent_workspace(agents_dir: &std::path::Path, agent_id: &str) -> std::path::PathBuf {
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {}\n", agent_id)).unwrap();
    agent_dir
}

#[tokio::test]
async fn ri_0_9_records_last_word_response_after_notice_delivered_and_turn_completes() {
    let (config, store, execution) = setup();
    let session_id = "sess-ri09-response";
    let agent_id = "ri09.agent";
    let agent_dir = seed_agent_workspace(&config.agents_dir, agent_id);

    execution
        .degrade_session_with_options(session_id, "operator_degrade", true)
        .await
        .unwrap();

    let notice_event = store
        .search_causal_events(Some(session_id), None, 64)
        .unwrap()
        .into_iter()
        .find(|e| e.action == "session.last_word_notice")
        .expect("last_word_notice");
    let notice_payload: serde_json::Value =
        serde_json::from_str(notice_event.payload.as_deref().unwrap()).unwrap();
    let notice_msg_id = notice_payload
        .get("notice_message_id")
        .and_then(|v| v.as_str())
        .expect("notice_message_id in payload");

    let ds = execution.degraded_sessions();
    let mut runtime = AgentExecutor::new(
        manifest_simple(agent_id),
        "You are a test agent.".to_string(),
        std::sync::Arc::new(FixedReplyDriver {
            text: "Final words from the agent before standing down.",
        }),
        agent_dir,
        default_registry(),
        Some(store.clone()),
    )
    .with_gateway_dir(config.agents_dir.join(".gateway"))
    .with_session_id(session_id.to_string())
    .with_degraded_sessions(Some(ds));

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Continue.".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let resp_event = store
        .search_causal_events(Some(session_id), None, 128)
        .unwrap()
        .into_iter()
        .find(|e| e.action == "session.last_word_response")
        .expect("session.last_word_response causal event");
    assert!(
        resp_event.enforced_rules.contains(&"Ri-0.9".to_string()),
        "response event must cite Ri-0.9"
    );
    let body: serde_json::Value =
        serde_json::from_str(resp_event.payload.as_deref().unwrap()).unwrap();
    let ids = body
        .get("notice_message_ids")
        .and_then(|v| v.as_array())
        .expect("notice_message_ids");
    assert!(
        ids.iter().any(|v| v.as_str() == Some(notice_msg_id)),
        "response should reference the Ri-0.9 notice message id"
    );
    assert_eq!(
        body.get("assistant_reply_present")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    let preview = body
        .get("assistant_reply_preview")
        .and_then(|v| v.as_str())
        .expect("preview");
    assert!(preview.contains("Final words from the agent"));
}
