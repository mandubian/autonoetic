//! Integration tests for session checkpoint system.
//!
//! Tests cover checkpoint creation, loading, pruning, and fork-from-checkpoint behavior.

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    delete_checkpoint, list_checkpoints, load_checkpoint, load_latest_checkpoint,
    prune_checkpoints, save_checkpoint, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::LoopGuardState;
use autonoetic_gateway::runtime::session_snapshot::SessionFork;
use autonoetic_types::config::GatewayConfig;

/// Helper to create a test config with temp directory.
fn test_config(temp: &tempfile::TempDir) -> GatewayConfig {
    GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    }
}

fn default_guard_state() -> LoopGuardState {
    LoopGuardState {
        max_loops_without_progress: 10,
        current_loops: 0,
        last_failure_hash: None,
        consecutive_failures: 0,
    }
}

fn make_checkpoint(
    session_id: &str,
    turn_id: &str,
    turn_counter: u64,
    yield_reason: YieldReason,
) -> SessionCheckpoint {
    SessionCheckpoint {
        history: vec![
            Message::system("You are a test agent"),
            Message::user("Hello, test"),
            Message::assistant("Test response"),
        ],
        turn_counter,
        loop_guard_state: default_guard_state(),
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: Some("abc123hash".to_string()),
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason,
        content_store_refs: vec![],
        created_at: "2024-01-01T00:00:00Z".to_string(),
        pending_tool_state: None,
        llm_rounds_consumed: 1,
        tool_invocations_consumed: 0,
        tokens_consumed: 100,
        estimated_cost_usd: 0.001,
    }
}

/// Test checkpoint save and load round-trip.
#[test]
fn test_checkpoint_save_and_load() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);
    let session_id = "ckpt-session-1";

    let checkpoint = make_checkpoint(session_id, "turn-001", 1, YieldReason::Hibernation);

    save_checkpoint(&config, &checkpoint).expect("save should succeed");

    let loaded = load_checkpoint(&config, session_id, "turn-001")
        .expect("load should succeed")
        .expect("checkpoint should exist");

    assert_eq!(loaded.session_id, session_id);
    assert_eq!(loaded.turn_counter, 1);
    assert_eq!(loaded.turn_id, "turn-001");
    assert_eq!(loaded.yield_reason, YieldReason::Hibernation);
    assert_eq!(loaded.history.len(), 3);
    assert_eq!(loaded.runtime_lock_hash, Some("abc123hash".to_string()));
}

/// Test loading latest checkpoint returns the highest turn.
#[test]
fn test_checkpoint_load_latest() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);
    let session_id = "ckpt-session-2";

    // Save checkpoints for turns 1, 2, 3
    for i in 1..=3 {
        let checkpoint = make_checkpoint(
            session_id,
            &format!("turn-{:03}", i),
            i,
            YieldReason::Hibernation,
        );
        save_checkpoint(&config, &checkpoint).unwrap();
    }

    let latest = load_latest_checkpoint(&config, session_id)
        .expect("load should succeed")
        .expect("checkpoint should exist");

    assert_eq!(latest.turn_counter, 3);
    assert_eq!(latest.turn_id, "turn-003");
}

/// Test pruning keeps only last N checkpoints.
#[test]
fn test_checkpoint_pruning() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);
    let session_id = "ckpt-session-3";

    // Save 5 checkpoints
    for i in 1..=5 {
        let checkpoint = make_checkpoint(
            session_id,
            &format!("turn-{:03}", i),
            i,
            YieldReason::Hibernation,
        );
        save_checkpoint(&config, &checkpoint).unwrap();
    }

    // Prune, keeping last 3
    prune_checkpoints(&config, session_id, 3).unwrap();

    let remaining = list_checkpoints(&config, session_id).unwrap();
    assert_eq!(remaining.len(), 3);
    assert!(remaining.contains(&"turn-003".to_string()));
    assert!(remaining.contains(&"turn-004".to_string()));
    assert!(remaining.contains(&"turn-005".to_string()));
}

/// Test checkpoint deletion.
#[test]
fn test_checkpoint_deletion() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);
    let session_id = "ckpt-session-4";

    let checkpoint = make_checkpoint(session_id, "turn-001", 1, YieldReason::Hibernation);
    save_checkpoint(&config, &checkpoint).unwrap();

    // Verify it exists
    assert!(load_checkpoint(&config, session_id, "turn-001")
        .unwrap()
        .is_some());

    // Delete it
    delete_checkpoint(&config, session_id, "turn-001").unwrap();

    // Verify it's gone
    assert!(load_checkpoint(&config, session_id, "turn-001")
        .unwrap()
        .is_none());
}

/// Test fork from checkpoint preserves history.
#[test]
fn test_fork_from_checkpoint_preserves_history() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let checkpoint = make_checkpoint("original-session", "turn-002", 2, YieldReason::Hibernation);

    let fork = SessionFork::fork_from_checkpoint(
        &checkpoint,
        Some("forked-session"),
        Some("Try a different approach"),
        &gateway_dir,
    )
    .expect("fork should succeed");

    assert_eq!(fork.new_session_id, "forked-session");
    assert_eq!(fork.source_session_id, "original-session");
    assert_eq!(fork.fork_turn, 2);

    // Original 3 messages + 1 branch message
    assert_eq!(fork.initial_history.len(), 4);
    assert_eq!(fork.initial_history[3].content, "Try a different approach");
}

/// Test fork from checkpoint without branch message.
#[test]
fn test_fork_from_checkpoint_no_branch() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let checkpoint = make_checkpoint(
        "source-session",
        "turn-003",
        3,
        YieldReason::MaxTurnsReached,
    );

    let fork = SessionFork::fork_from_checkpoint(
        &checkpoint,
        Some("continued-session"),
        None, // No branch message
        &gateway_dir,
    )
    .expect("fork should succeed");

    // Same as original history (no branch message added)
    assert_eq!(fork.initial_history.len(), 3);
    assert_eq!(fork.source_session_id, "source-session");
}

/// Test checkpoint with different yield reasons round-trips correctly.
#[test]
fn test_checkpoint_yield_reasons() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);

    let reasons = vec![
        YieldReason::Hibernation,
        YieldReason::BudgetExhausted,
        YieldReason::ApprovalRequired {
            approval_request_id: "apr-123".to_string(),
        },
        YieldReason::MaxTurnsReached,
        YieldReason::ManualStop,
        YieldReason::Error("test error".to_string()),
    ];

    for (i, reason) in reasons.into_iter().enumerate() {
        let session_id = format!("yield-session-{}", i);
        let checkpoint = make_checkpoint(
            &session_id,
            &format!("turn-{:03}", i),
            i as u64,
            reason.clone(),
        );
        save_checkpoint(&config, &checkpoint).unwrap();

        let loaded = load_checkpoint(&config, &session_id, &format!("turn-{:03}", i))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.yield_reason, reason);
    }
}

/// Test that EmergencyStop checkpoints have the correct shape.
#[test]
fn test_checkpoint_emergency_stop() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);

    let checkpoint = make_checkpoint(
        "estop-session",
        "turn-005",
        5,
        YieldReason::EmergencyStop {
            stop_id: "estop-abc123".to_string(),
        },
    );
    save_checkpoint(&config, &checkpoint).unwrap();

    let loaded = load_checkpoint(&config, "estop-session", "turn-005")
        .unwrap()
        .unwrap();

    match &loaded.yield_reason {
        YieldReason::EmergencyStop { stop_id } => {
            assert_eq!(stop_id, "estop-abc123");
        }
        other => panic!("Expected EmergencyStop, got {:?}", other),
    }
}

/// Test checkpoint with pending tool state.
#[test]
fn test_checkpoint_with_pending_tool_state() {
    use autonoetic_gateway::runtime::checkpoint::{PendingToolCall, PendingToolState};

    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);

    let mut checkpoint = make_checkpoint(
        "tool-session",
        "turn-001",
        1,
        YieldReason::ApprovalRequired {
            approval_request_id: "apr-456".to_string(),
        },
    );

    checkpoint.pending_tool_state = Some(PendingToolState {
        completed_tool_results: vec![(
            "call-1".to_string(),
            "sandbox.exec".to_string(),
            r#"{"ok": true}"#.to_string(),
        )],
        pending_tool_call: PendingToolCall {
            call_id: "call-2".to_string(),
            tool_name: "agent.install".to_string(),
            arguments: r#"{"agent_id": "test"}"#.to_string(),
            approval_response: Some(r#"{"approval_required": true}"#.to_string()),
        },
        remaining_tool_calls: vec![],
    });

    save_checkpoint(&config, &checkpoint).unwrap();

    let loaded = load_checkpoint(&config, "tool-session", "turn-001")
        .unwrap()
        .unwrap();

    let pending = loaded
        .pending_tool_state
        .expect("should have pending state");
    assert_eq!(pending.completed_tool_results.len(), 1);
    assert_eq!(pending.pending_tool_call.call_id, "call-2");
    assert_eq!(pending.pending_tool_call.tool_name, "agent.install");
}

/// Test checkpoint with budget tracking.
#[test]
fn test_checkpoint_budget_tracking() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);

    let mut checkpoint = make_checkpoint(
        "budget-session",
        "turn-003",
        3,
        YieldReason::BudgetExhausted,
    );
    checkpoint.llm_rounds_consumed = 15;
    checkpoint.tokens_consumed = 50000;
    checkpoint.estimated_cost_usd = 0.25;

    save_checkpoint(&config, &checkpoint).unwrap();

    let loaded = load_checkpoint(&config, "budget-session", "turn-003")
        .unwrap()
        .unwrap();

    assert_eq!(loaded.llm_rounds_consumed, 15);
    assert_eq!(loaded.tokens_consumed, 50000);
    assert!((loaded.estimated_cost_usd - 0.25).abs() < 0.001);
    assert_eq!(loaded.yield_reason, YieldReason::BudgetExhausted);
}

/// Test that non-existent checkpoint returns None.
#[test]
fn test_checkpoint_load_nonexistent() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);

    let result = load_checkpoint(&config, "nonexistent-session", "turn-001").unwrap();
    assert!(result.is_none());

    let latest = load_latest_checkpoint(&config, "nonexistent-session").unwrap();
    assert!(latest.is_none());
}

/// Test listing checkpoints for a session.
#[test]
fn test_checkpoint_listing() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config = test_config(&temp);
    let session_id = "list-session";

    // No checkpoints initially
    let empty = list_checkpoints(&config, session_id).unwrap();
    assert!(empty.is_empty());

    // Add some checkpoints
    for i in [1, 3, 5] {
        let checkpoint = make_checkpoint(
            session_id,
            &format!("turn-{:03}", i),
            i as u64,
            YieldReason::Hibernation,
        );
        save_checkpoint(&config, &checkpoint).unwrap();
    }

    let ids = list_checkpoints(&config, session_id).unwrap();
    assert_eq!(ids.len(), 3);
    // Should be sorted by turn number
    assert_eq!(ids[0], "turn-001");
    assert_eq!(ids[1], "turn-003");
    assert_eq!(ids[2], "turn-005");
}
