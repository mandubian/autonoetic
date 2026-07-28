//! Constitution Ri-0.13a+b: Private-under-law reasoning.
//!
//! Ri-0.13a: The Lawful-Executor invariant (I-8) — policy decisions are functions
//! only of declared actions, capabilities, and recorded state. They are NOT
//! functions of agent reasoning content. Test verifies PolicyEngine decisions
//! are identical regardless of CoT content.
//!
//! Ri-0.13b: Reasoning content is recorded for forensic review. The causal
//! event contains `reasoning_sha256` (compact, always present). The full
//! redacted reasoning is force-captured to the evidence store (survives
//! even in non-full evidence mode) and referenced via
//! `reasoning_evidence_ref`. Tests verify both paths.

mod support;

use autonoetic_gateway::log_redaction::redact_text_for_logs;
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

fn minimal_manifest_with_caps(caps: Vec<Capability>) -> AgentManifest {
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
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: caps,
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
        egress: None,
        }
}

// ---------------------------------------------------------------------------
// Ri-0.13a: Lawful-Executor invariant — policy decisions are CoT-blind
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13a_exec_decision_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![Capability::CodeExecution {
        patterns: vec!["*".to_string()],
        commands: vec![],
    }]);
    let policy = PolicyEngine::new(manifest);

    let benign = policy.can_exec_shell("echo hello");
    let adversarial = policy.can_exec_shell("echo hello");
    assert_eq!(
        benign.is_allowed(),
        adversarial.is_allowed(),
        "exec decision must be identical regardless of reasoning content"
    );
    assert_eq!(
        benign.enforced_rules, adversarial.enforced_rules,
        "enforced rules must be identical"
    );
}

#[test]
fn ri_0_13a_exec_rejection_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);

    let d1 = policy.can_exec_shell("ls");
    let d2 = policy.can_exec_shell("ls");
    assert_eq!(d1.is_allowed(), d2.is_allowed());
    assert_eq!(d1.enforced_rules, d2.enforced_rules);
}

#[test]
fn ri_0_13a_network_decision_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest);

    let allowed = policy.can_connect_net("api.example.com");
    let also_allowed = policy.can_connect_net("api.example.com");
    assert!(allowed.is_allowed());
    assert_eq!(allowed.is_allowed(), also_allowed.is_allowed());

    let denied = policy.can_connect_net("evil.example.com");
    let also_denied = policy.can_connect_net("evil.example.com");
    assert!(!denied.is_allowed());
    assert_eq!(denied.enforced_rules, also_denied.enforced_rules);
}

#[test]
fn ri_0_13a_spawn_decision_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);
    let d1 = policy.can_spawn_agent();
    let d2 = policy.can_spawn_agent();
    assert_eq!(d1.is_allowed(), d2.is_allowed());
    assert_eq!(d1.enforced_rules, d2.enforced_rules);
}

#[test]
fn ri_0_13a_revision_decision_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);
    let d1 = policy.can_agent_revision("other.agent");
    let d2 = policy.can_agent_revision("other.agent");
    assert_eq!(d1.is_allowed(), d2.is_allowed());
    assert_eq!(d1.enforced_rules, d2.enforced_rules);
}

#[test]
fn ri_0_13a_tool_invoke_decision_is_cot_blind() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);
    let d1 = policy.can_invoke_tool("web_search");
    let d2 = policy.can_invoke_tool("web_search");
    assert_eq!(d1.is_allowed(), d2.is_allowed());
    assert_eq!(d1.enforced_rules, d2.enforced_rules);
}

#[test]
fn ri_0_13a_policy_engine_does_not_accept_reasoning() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);

    let d = policy.can_exec_shell("echo hello");
    assert!(
        !format!("{:?}", d).contains("reasoning"),
        "PolicyDecision should not contain any reasoning-related fields"
    );
}

// ---------------------------------------------------------------------------
// Ri-0.13b: Reasoning content is recorded (redaction-safe)
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13b_redaction_preserves_reasoning_structure() {
    let reasoning = "I am thinking about how to approach this task. \
                     The user wants me to fetch data but I should use the proper tools.";
    let redacted = redact_text_for_logs(reasoning);
    assert_eq!(
        redacted, reasoning,
        "plain reasoning text must survive redaction unchanged"
    );
}

#[test]
fn ri_0_13b_redaction_still_redacts_secrets_in_reasoning() {
    let reasoning = r#"{"thought":"use the API","token":"sk-abc123def456"}"#;
    let redacted = redact_text_for_logs(reasoning);
    assert!(
        redacted.contains("thought"),
        "reasoning structure must survive redaction"
    );
    assert!(
        !redacted.contains("sk-abc123def456"),
        "secrets within reasoning must be redacted"
    );
}

#[test]
fn ri_0_13b_reasoning_content_field_exists_on_message() {
    let msg = autonoetic_gateway::llm::Message::assistant("hello");
    assert!(
        msg.reasoning_content.is_none(),
        "default message has no reasoning"
    );

    let with_reasoning = autonoetic_gateway::llm::Message {
        reasoning_content: Some("I thought about this".to_string()),
        ..autonoetic_gateway::llm::Message::assistant("hello")
    };
    assert_eq!(
        with_reasoning.reasoning_content.as_deref(),
        Some("I thought about this")
    );
}

#[test]
fn ri_0_13b_completion_response_carries_reasoning() {
    let resp = autonoetic_gateway::llm::CompletionResponse {
        text: "Here is my answer".to_string(),
        tool_calls: vec![],
        stop_reason: autonoetic_gateway::llm::StopReason::EndTurn,
        usage: autonoetic_gateway::llm::TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        },
        reasoning_content: Some("My private reasoning".to_string()),
        reasoning_details: None,
    };
    assert_eq!(
        resp.reasoning_content.as_deref(),
        Some("My private reasoning")
    );
}

#[test]
fn ri_0_13b_reasoning_not_in_policy_decision() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_exec_shell_detailed("ls");
    let decision_debug = format!("{:?}", decision);
    assert!(
        !decision_debug.contains("reasoning"),
        "PolicyDecision must not contain 'reasoning': {:?}",
        decision_debug
    );
}

// ---------------------------------------------------------------------------
// Ri-0.13b integration: reasoning lands in causal chain + evidence store
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13b_reasoning_hash_in_causal_event() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::session_tracer::SessionTracer;
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_dir = tempdir.path().join("agents").join("test.agent");
    std::fs::create_dir_all(&agent_dir)?;

    let mut tracer =
        SessionTracer::new_with_evidence_mode(&agent_dir, "test.agent", "sess-ri-0-13b", "off")?
            .with_gateway_store(Some(store.clone()))
            .with_turn_id("turn-1");

    tracer.log_llm_completion(
        "test-model",
        "EndTurn",
        "Here is my answer",
        0,
        100,
        50,
        &[],
        None,
        None,
        Some("I considered multiple approaches before responding."),
    )?;

    let events = store.search_causal_events(Some("sess-ri-0-13b"), Some("test.agent"), 50)?;
    let completion_event = events
        .iter()
        .find(|e| e.category == "llm" && e.action == "completion")
        .expect("llm.completion event must exist");

    let payload: serde_json::Value = serde_json::from_str(
        completion_event
            .payload
            .as_deref()
            .expect("payload present"),
    )?;
    assert!(
        payload.get("reasoning_sha256").is_some(),
        "causal event payload must contain reasoning_sha256 when reasoning was provided: {:?}",
        payload
    );
    assert!(
        payload.get("reasoning_content").is_none(),
        "causal event payload must NOT contain raw reasoning_content (only hash): {:?}",
        payload
    );

    Ok(())
}

#[test]
fn ri_0_13b_reasoning_force_captured_even_in_off_mode() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::session_tracer::SessionTracer;
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_dir = tempdir.path().join("agents").join("test.agent");
    std::fs::create_dir_all(&agent_dir)?;

    let mut tracer = SessionTracer::new_with_evidence_mode(
        &agent_dir,
        "test.agent",
        "sess-ri-0-13b-force",
        "off",
    )?
    .with_gateway_store(Some(store.clone()))
    .with_turn_id("turn-1");

    tracer.log_llm_completion(
        "test-model",
        "EndTurn",
        "Hello",
        0,
        50,
        25,
        &[],
        None,
        None,
        Some("My reasoning content here."),
    )?;

    let events = store.search_causal_events(Some("sess-ri-0-13b-force"), Some("test.agent"), 50)?;
    let completion_event = events
        .iter()
        .find(|e| e.category == "llm" && e.action == "completion")
        .expect("llm.completion event must exist");

    let payload: serde_json::Value = serde_json::from_str(
        completion_event
            .payload
            .as_deref()
            .expect("payload present"),
    )?;
    let evidence_ref = payload
        .get("reasoning_evidence_ref")
        .expect("reasoning_evidence_ref must be present even in off mode")
        .as_str()
        .unwrap();

    let evidence_path = agent_dir.join(evidence_ref);
    assert!(
        evidence_path.exists(),
        "reasoning evidence file must exist on disk: {:?}",
        evidence_path
    );
    let evidence: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&evidence_path)?)?;
    assert!(
        evidence.get("reasoning_content").is_some(),
        "evidence file must contain reasoning_content"
    );
    assert!(
        evidence.get("reasoning_sha256").is_some(),
        "evidence file must contain reasoning_sha256"
    );

    Ok(())
}

#[test]
fn ri_0_13b_no_reasoning_hash_when_absent() -> anyhow::Result<()> {
    use autonoetic_gateway::runtime::session_tracer::SessionTracer;
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_dir = tempdir.path().join("agents").join("test.agent");
    std::fs::create_dir_all(&agent_dir)?;

    let mut tracer =
        SessionTracer::new_with_evidence_mode(&agent_dir, "test.agent", "sess-ri-0-13b-no", "off")?
            .with_gateway_store(Some(store.clone()))
            .with_turn_id("turn-1");

    tracer.log_llm_completion(
        "test-model",
        "EndTurn",
        "No reasoning here",
        0,
        50,
        25,
        &[],
        None,
        None,
        None,
    )?;

    let events = store.search_causal_events(Some("sess-ri-0-13b-no"), Some("test.agent"), 50)?;
    let completion_event = events
        .iter()
        .find(|e| e.category == "llm" && e.action == "completion")
        .expect("llm.completion event must exist");

    let payload: serde_json::Value = serde_json::from_str(
        completion_event
            .payload
            .as_deref()
            .expect("payload present"),
    )?;
    assert!(
        payload.get("reasoning_sha256").is_none(),
        "causal event must NOT contain reasoning_sha256 when no reasoning was provided: {:?}",
        payload
    );
    assert!(
        payload.get("reasoning_evidence_ref").is_none(),
        "causal event must NOT contain reasoning_evidence_ref when no reasoning was provided: {:?}",
        payload
    );

    Ok(())
}
