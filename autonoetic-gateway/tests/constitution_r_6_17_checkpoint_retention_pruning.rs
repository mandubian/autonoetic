//! Constitution P-6.17 — checkpoint retention pruning keeps only configured tail.

mod support;

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    list_checkpoints, prune_checkpoints, save_checkpoint, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::LoopGuardState;

fn make_checkpoint(session_id: &str, turn: u64) -> SessionCheckpoint {
    SessionCheckpoint {
        history: vec![Message::user(format!("turn-{turn}"))],
        turn_counter: turn,
        loop_guard_state: LoopGuardState {
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
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: format!("turn-{turn:04}"),
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
    }
}

#[test]
fn r_6_17_prune_checkpoints_respects_keep_last() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let session_id = "session-r-6-17";

    save_checkpoint(&config, &make_checkpoint(session_id, 1))?;
    save_checkpoint(&config, &make_checkpoint(session_id, 2))?;
    save_checkpoint(&config, &make_checkpoint(session_id, 3))?;

    prune_checkpoints(&config, session_id, 2)?;
    let remaining = list_checkpoints(&config, session_id)?;

    assert_eq!(
        remaining,
        vec!["turn-0002".to_string(), "turn-0003".to_string()],
        "pruning must keep only the most recent checkpoints"
    );
    Ok(())
}
