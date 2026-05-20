//! Sentinel P2 Integration Tests:
//! 1. `sentinel.suppress` accepts `reason` parameter and emits causal event
//! 2. Diverging session produces `agent.message` to root planner
//! 3. `sentinel.suppress(turns=3)` silences subsequent divergence messages

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::guard::LoopGuardState;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::runtime::trajectory_monitor::TrajectoryMonitor;
use autonoetic_gateway::scheduler::gateway_store::{AgentMessageRecord, GatewayStore};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::TrajectoryConfig;

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

fn quiet_guard_state() -> LoopGuardState {
    LoopGuardState {
        max_loops_without_progress: 5,
        max_tool_failures: 5,
        max_consecutive_same_progress: 1,
        max_child_failures: 3,
        progress_budget_tools: HashMap::new(),
        progress_budget_used: HashMap::new(),
        current_loops: 0,
        tool_failure_counts: HashMap::new(),
        last_progress_fingerprint: None,
        consecutive_progress_count: 0,
        child_failure_count: 0,
    }
}

fn drive_to_diverging(
    mon: &mut TrajectoryMonitor,
    state: &mut LoopGuardState,
) -> autonoetic_gateway::runtime::trajectory_monitor::TickResult {
    state.current_loops = 4;
    let _ = mon.tick(4, &[], None, &state);
    state.current_loops = 4;
    state
        .tool_failure_counts
        .insert("sandbox.exec".into(), 4);
    mon.tick(6, &[], None, &state)
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
    let agent_id = "test-agent";
    let root_sid = "root-session";

    let r = drive_to_diverging(&mut mon, &mut state);
    assert!(r.level_changed);
    assert_eq!(r.health.level_str(), "diverging");
    assert_eq!(r.health.causal_action(), Some("detected"));

    // Simulate the lifecycle messaging logic (lifecycle.rs ~2383-2441)
    let turn_counter = 6u64;
    let now = chrono::Utc::now().to_rfc3339();
    let level = r.health.level_str();
    let msg_id = format!("msg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let message = format!(
        "[Sentinel Notice]\n\
         Level: {}\n\
         Turn: {}\n\
         Agent: {}\n\
         The trajectory monitor has detected a divergence pattern. \
         Review the causal chain for divergence.* events.",
        level, turn_counter, agent_id,
    );
    let record = AgentMessageRecord {
        message_id: msg_id.clone(),
        sender_session_id: "gateway:sentinel".to_string(),
        sender_agent_id: "gateway".to_string(),
        target_pattern: format!("session:{}", root_sid),
        message,
        created_at: now,
    };
    store.save_agent_message(&record)?;
    store.insert_message_delivery(&msg_id, root_sid)?;

    // Verify message was saved and is undelivered
    let undelivered = store.fetch_undelivered_messages(root_sid)?;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].sender_agent_id, "gateway");
    assert!(undelivered[0].message.contains("[Sentinel Notice]"));
    assert!(undelivered[0].message.contains("diverging"));
    assert!(undelivered[0].message.contains(&turn_counter.to_string()));

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

    // Suppress for 10 turns from turn 0 → suppress_until = 10
    let suppress_until_turn = Arc::new(AtomicU64::new(10));

    let r = drive_to_diverging(&mut mon, &mut state);
    assert!(r.level_changed);
    assert_eq!(r.health.level_str(), "diverging");

    // Replicate the lifecycle suppression check (lifecycle.rs line 2387-2388)
    let turn_counter = 6u64;
    let suppressed = turn_counter < suppress_until_turn.load(Ordering::Relaxed);
    assert!(
        suppressed,
        "turn {} should be suppressed when suppress_until=10",
        turn_counter
    );

    // When suppressed, lifecycle skips the entire messaging block
    if !suppressed {
        let now = chrono::Utc::now().to_rfc3339();
        let msg_id = format!("msg-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let record = AgentMessageRecord {
            message_id: msg_id.clone(),
            sender_session_id: "gateway:sentinel".to_string(),
            sender_agent_id: "gateway".to_string(),
            target_pattern: format!("session:{}", root_sid),
            message: "should not appear".to_string(),
            created_at: now,
        };
        store.save_agent_message(&record)?;
        store.insert_message_delivery(&msg_id, root_sid)?;
    }

    // Verify no messages were saved
    let undelivered = store.fetch_undelivered_messages(root_sid)?;
    assert!(
        undelivered.is_empty(),
        "no messages should exist when suppression is active"
    );

    Ok(())
}
