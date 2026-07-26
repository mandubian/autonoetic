mod support;

use autonoetic_gateway::llm::ToolCall;
use autonoetic_gateway::runtime::disclosure::DisclosureState;
use autonoetic_gateway::runtime::mcp::McpToolRuntime;
use autonoetic_gateway::runtime::session_tracer::SessionTracer;
use autonoetic_gateway::runtime::tool_call_processor::ToolCallProcessor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use std::sync::Arc;
use tempfile::tempdir;

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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
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

#[tokio::test]
async fn privileged_tool_without_intent_rejects_and_with_intent_is_captured() -> anyhow::Result<()>
{
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("test-agent");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(agent_dir.join("history"))?;
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let manifest = test_manifest();
    let registry = default_registry();
    let mut mcp_runtime = McpToolRuntime::empty();
    let mut disclosure_state = DisclosureState::default();
    let mut tracer = SessionTracer::new(&agent_dir, "test-agent", "test-session")?
        .with_gateway_store(Some(store.clone()));
    tracer.set_turn_id("turn-000001");

    let mut processor = ToolCallProcessor::new(
        &mut mcp_runtime,
        &registry,
        &manifest,
        &mut disclosure_state,
        None,
        None,
        Some(store.clone()),
        None,
    )
    .with_session_context(
        Some("test-session".to_string()),
        Some("turn-000001".to_string()),
    );

    let missing_intent = vec![ToolCall {
        id: "tc-no-intent".to_string(),
        name: "sandbox_exec".to_string(),
        arguments: r#"{"command":"echo hello"}"#.to_string(),
    }];
    let (had_success_missing, results_missing) = processor
        .process_tool_calls(&missing_intent, &agent_dir, Some(&gateway_dir), &mut tracer)
        .await?;

    assert!(!had_success_missing);
    assert_eq!(results_missing.len(), 1);
    let missing_json: serde_json::Value = serde_json::from_str(&results_missing[0].2)?;
    assert_eq!(missing_json["ok"], false);
    assert_eq!(missing_json["error_type"], "validation");
    assert!(missing_json["message"]
        .as_str()
        .unwrap_or_default()
        .contains("intent_required"));

    let intent_text =
        "Need to run a harmless shell probe to inspect the workspace state before editing.";
    let with_intent = vec![ToolCall {
        id: "tc-with-intent".to_string(),
        name: "sandbox_exec".to_string(),
        arguments: format!(
            "{{\"command\":\"echo hello\",\"intent\":{}}}",
            serde_json::to_string(intent_text)?
        ),
    }];
    let _ = processor
        .process_tool_calls(&with_intent, &agent_dir, Some(&gateway_dir), &mut tracer)
        .await?;

    let events = store.search_causal_events(Some("test-session"), Some("test-agent"), 100)?;
    let requested_event = events
        .iter()
        .find(|event| event.category == "tool_invoke" && event.action == "requested")
        .expect("tool_invoke requested event should exist");
    let payload: serde_json::Value = serde_json::from_str(
        requested_event
            .payload
            .as_deref()
            .expect("payload should be present"),
    )?;

    assert_eq!(payload["tool_name"], "sandbox_exec");
    assert_eq!(payload["intent"], intent_text);

    Ok(())
}
