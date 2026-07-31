//! Constitution P-6.14 — EmergencyStop checkpoints never auto-resume.


use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::GatewayExecutionService;

fn checkpoint_with_emergency_stop(session_id: &str) -> SessionCheckpoint {
    SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![Message::user("hello")],
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
        session_state: Default::default(),
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-0001".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::EmergencyStop {
            stop_id: "stop-r-6-14".to_string(),
        },
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
    }
}

#[tokio::test]
async fn r_6_14_emergency_stop_checkpoint_cannot_auto_resume() -> anyhow::Result<()> {
    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let session_id = "session-r-6-14";

    let checkpoint = checkpoint_with_emergency_stop(session_id);
    save_checkpoint(&config, &checkpoint)?;

    let execution = GatewayExecutionService::new(config, None);
    let err = execution
        .respawn_from_checkpoint("any-agent", session_id, None, None, None, None, &[])
        .await
        .expect_err("EmergencyStop checkpoint must never auto-resume");

    assert!(
        err.to_string()
            .contains("Cannot auto-resume from EmergencyStop checkpoint"),
        "unexpected error: {err}"
    );

    Ok(())
}
