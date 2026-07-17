//! Sentinel P2 Integration Tests:
//! 1. `sentinel.suppress` accepts `reason` parameter and emits causal event
//! 2. Diverging session produces `agent.message` to root planner
//! 3. `sentinel.suppress(turns=3)` silences subsequent divergence messages

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::runtime::trajectory_monitor::TrajectoryMonitor;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::TrajectoryConfig;
use autonoetic_types::trajectory::FeedbackEvent;

fn test_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: autonoetic_types::agent::RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: autonoetic_types::agent::AgentIdentity {
            id: "test-agent".to_string(),
            name: "test".to_string(),
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

fn quiet_guard_state() -> LoopGuard {
    LoopGuard::default()
}

fn drive_to_diverging(
    mon: &mut TrajectoryMonitor,
    state: &mut LoopGuard,
) -> autonoetic_gateway::runtime::trajectory_monitor::TickResult {
    // Under RFC D.6 only FeedbackIgnored can drive Diverging/Critical.
    // Build the ignored signal by issuing feedback on turn 4 and repeating
    // the same feedback event on turn 6.
    let fb = FeedbackEvent::Validation {
        rule: "output_schema".into(),
        field_path: None,
    };
    mon.record_feedback(4, &[fb.clone()]);
    mon.record_feedback(5, &[fb.clone()]);
    mon.tick(5, &[], &[], None, state);
    mon.tick(6, &[], &[fb], None, state)
}

// ── Test 1: sentinel.suppress accepts reason and emits causal event ──────────

#[test]
fn sentinel_suppress_accepts_reason_and_emits_causal_event() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tempdir.path())?);
    let registry = default_registry();
    let man = test_manifest();
    let policy = PolicyEngine::new(man.clone());

    let target = Arc::new(AtomicU64::new(0));
    let exec_registry = ActiveExecutionRegistry::new();
    let run_context = NativeToolRunContext {
        registry: exec_registry,
        root_session_id: "root-test-session".to_string(),
        workflow_id: None,
        task_id: None,
        session_id: "test-session".to_string(),
        agent_id: man.agent.id.clone(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: Some(target.clone()),
        discovered_tools: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
    };

    let args = r#"{"turns": 3, "reason": "Testing suppression"}"#;
    let result = registry.execute(
        "sentinel_suppress",
        &man,
        &policy,
        Path::new("/tmp"),
        None,
        args,
        Some("test-session"),
        Some("turn-5"),
        None,
        Some(store.clone()),
        Some(&run_context),
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["reason"], serde_json::json!("Testing suppression"));
    assert_eq!(parsed["suppressed_for_turns"], 3);
    assert_eq!(parsed["suppress_until_turn"], 8);

    // Verify causal event was emitted
    let events = store.search_causal_events(Some("test-session"), Some("test-agent"), 100)?;
    let suppress_event = events
        .iter()
        .find(|e| e.category == "sentinel" && e.action == "suppress_activated")
        .expect("expected sentinel.suppress_activated causal event");
    assert_eq!(
        suppress_event.reason.as_deref(),
        Some("Testing suppression")
    );
    assert_eq!(suppress_event.turn_id.as_deref(), Some("turn-5"));

    Ok(())
}

// ── Test 2: diverging session sends agent.message to root planner ───────────

#[test]
fn diverging_session_sends_agent_message_to_root() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tempdir.path())?);
    let mut mon = TrajectoryMonitor::new(TrajectoryConfig::default());
    let mut state = quiet_guard_state();
    let root_sid = "root-session";
    let suppress_until = AtomicU64::new(0); // no suppression

    let r = drive_to_diverging(&mut mon, &mut state);
    assert!(r.level_changed);
    assert_eq!(r.health.level_str(), "diverging");
    assert_eq!(r.health.causal_action(), Some("detected"));

    // Use the shared lifecycle helper (same code path the lifecycle uses)
    let sent = AgentExecutor::send_divergence_notice(
        &store,
        root_sid,
        6, // turn_counter
        "test-agent",
        r.health.level_str(),
        &suppress_until,
        true, // notify_planner
    );
    assert!(sent, "message should have been sent (no suppression)");

    // Verify message was saved and is undelivered
    let undelivered = store.fetch_undelivered_messages(root_sid)?;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].sender_agent_id, "gateway");
    assert!(undelivered[0].message.contains("[Sentinel Notice]"));
    assert!(undelivered[0].message.contains("diverging"));
    assert!(undelivered[0].message.contains("6")); // turn counter

    Ok(())
}

// ── Test 3: suppression prevents divergence messages ────────────────────────

#[test]
fn sentinel_suppress_prevents_divergence_messages() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tempdir.path())?);
    let mut mon = TrajectoryMonitor::new(TrajectoryConfig::default());
    let mut state = quiet_guard_state();
    let root_sid = "root-session";

    // Suppress for 10 turns → suppress_until = 10
    let suppress_until = AtomicU64::new(10);

    let r = drive_to_diverging(&mut mon, &mut state);
    assert!(r.level_changed);
    assert_eq!(r.health.level_str(), "diverging");

    // Use the shared lifecycle helper — suppression is checked internally
    let sent = AgentExecutor::send_divergence_notice(
        &store,
        root_sid,
        6, // turn_counter (suppressed since 6 < 10)
        "test-agent",
        r.health.level_str(),
        &suppress_until,
        true, // notify_planner
    );
    assert!(!sent, "message should NOT have been sent (suppress_until=10 > turn=6)");

    // Verify no messages were saved
    let undelivered = store.fetch_undelivered_messages(root_sid)?;
    assert!(
        undelivered.is_empty(),
        "no messages should exist when suppression is active"
    );

    // Also verify that without suppression the message IS sent (sanity check)
    let suppress_until = AtomicU64::new(0);
    let sent = AgentExecutor::send_divergence_notice(
        &store,
        root_sid,
        6,
        "test-agent",
        "diverging",
        &suppress_until,
        true,
    );
    assert!(sent, "without suppression, message should be sent");
    let undelivered = store.fetch_undelivered_messages(root_sid)?;
    assert_eq!(undelivered.len(), 1, "message should now exist");

    Ok(())
}
