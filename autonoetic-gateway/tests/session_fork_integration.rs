//! Integration tests for session forking from checkpoints.

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    list_checkpoints, load_latest_checkpoint, prune_checkpoints, save_checkpoint, turn_id_for,
    SessionCheckpoint, SessionFork, YieldReason,
};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn test_config(temp: &tempfile::TempDir) -> GatewayConfig {
    GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    }
}

fn test_checkpoint(
    session_id: &str,
    turn_id: &str,
    history: Vec<Message>,
    turn_counter: u64,
) -> SessionCheckpoint {
    SessionCheckpoint {
        history,
        turn_counter,
        session_state: Default::default(),
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        loop_guard_state: LoopGuard {
            max_loops_without_progress: 10,
            max_tool_failures: 5,
            max_consecutive_same_progress: 2,
            max_child_failures: 3,
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            ..Default::default()
        },
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: "2024-01-01T00:00:00Z".to_string(),
        pending_tool_state: None,
        llm_rounds_consumed: 1,
        tool_invocations_consumed: 0,
        tokens_consumed: 100,
        estimated_cost_usd: 0.001,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
    }
}

/// Test forking from the latest checkpoint of a session.
#[test]
fn test_fork_from_latest_checkpoint() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);
    let session_id = "original-session";

    let history = vec![
        Message::user("Hello"),
        Message::assistant("Hi! How can I help you?"),
        Message::user("What is the weather?"),
        Message::assistant("I'll check that for you."),
    ];

    // Save a checkpoint for turn 1
    let cp = test_checkpoint(session_id, "turn-0001", history.clone(), 1);
    save_checkpoint(&config, &cp).unwrap();

    // Fork from the latest checkpoint
    let fork = SessionFork::fork(&config, session_id, Some("forked-session"), None).unwrap();

    assert_eq!(fork.new_session_id, "forked-session");
    assert_eq!(fork.source_session_id, session_id);
    assert_eq!(fork.fork_turn, 1);
    assert_eq!(fork.initial_history.len(), 4);
    assert!(fork.history_handle.starts_with("sha256:"));

    // Verify history is stored in forked session
    let gw_dir = temp.path().join(".gateway");
    let store = ContentStore::new(&gw_dir).unwrap();
    let forked_history = store
        .read_by_name("forked-session", "session_history")
        .unwrap();
    let loaded: Vec<Message> = serde_json::from_slice(&forked_history).unwrap();
    assert_eq!(loaded.len(), 4);
}

/// Test fork with branch message appends to history.
#[test]
fn test_fork_with_branch_message() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let history = vec![Message::user("Question"), Message::assistant("Answer")];

    let cp = test_checkpoint("session-a", "turn-0001", history, 1);
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork(
        &config,
        "session-a",
        Some("session-b"),
        Some("Try a different approach"),
    )
    .unwrap();

    // History should have original + branch message
    assert_eq!(fork.initial_history.len(), 3);
    assert_eq!(fork.initial_history[2].content, "Try a different approach");

    // Verify stored history includes branch message
    let gw_dir = temp.path().join(".gateway");
    let store = ContentStore::new(&gw_dir).unwrap();
    let stored = store.read_by_name("session-b", "session_history").unwrap();
    let loaded: Vec<Message> = serde_json::from_slice(&stored).unwrap();
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[2].content, "Try a different approach");
}

/// Test fork lineage is tracked.
#[test]
fn test_fork_lineage_tracking() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let cp = test_checkpoint(
        "parent-session",
        "turn-0001",
        vec![Message::user("Start")],
        1,
    );
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork(
        &config,
        "parent-session",
        Some("child-session"),
        Some("Branch point"),
    )
    .unwrap();

    assert_eq!(fork.source_session_id, "parent-session");
    assert_eq!(fork.new_session_id, "child-session");
    assert_eq!(fork.fork_turn, 1);
    assert!(fork.history_handle.starts_with("sha256:"));
}

/// Test fork without branch message (clean copy).
#[test]
fn test_fork_without_branch_message() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let history = vec![Message::user("Q1"), Message::assistant("A1")];
    let cp = test_checkpoint("session-a", "turn-0001", history, 1);
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork(&config, "session-a", Some("session-b"), None).unwrap();

    assert_eq!(fork.initial_history.len(), 2);
    assert_eq!(fork.initial_history[0].content, "Q1");
    assert_eq!(fork.initial_history[1].content, "A1");
}

/// Test auto-generated session ID for fork.
#[test]
fn test_fork_auto_session_id() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let cp = test_checkpoint("original", "turn-0001", vec![Message::user("Test")], 1);
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork(&config, "original", None, None).unwrap();

    assert!(fork.new_session_id.starts_with("fork-"));
    assert_ne!(fork.new_session_id, "original");
}

/// Test multi-level fork (fork of a fork).
#[test]
fn test_multi_level_fork() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    // Original session checkpoint
    let cp_a = test_checkpoint("session-a", "turn-0001", vec![Message::user("Original")], 1);
    save_checkpoint(&config, &cp_a).unwrap();

    // Fork to session B
    let fork_b =
        SessionFork::fork(&config, "session-a", Some("session-b"), Some("Branch 1")).unwrap();

    // Save checkpoint for session B (simulating it ran)
    let cp_b = test_checkpoint("session-b", "turn-0001", fork_b.initial_history.clone(), 2);
    save_checkpoint(&config, &cp_b).unwrap();

    // Fork to session C
    let fork_c =
        SessionFork::fork(&config, "session-b", Some("session-c"), Some("Branch 2")).unwrap();

    // Session C should have: Original + Branch 1 + Branch 2 = 3 messages
    assert_eq!(fork_c.initial_history.len(), 3);
    assert_eq!(fork_c.initial_history[0].content, "Original");
    assert_eq!(fork_c.initial_history[1].content, "Branch 1");
    assert_eq!(fork_c.initial_history[2].content, "Branch 2");
}

/// A fork must be *runnable*: the execution engine seeds a session's LLM
/// context from its latest checkpoint, so forking has to write one under the
/// new session id (cloned history, marked Hibernation). Without it, the branch
/// would resume from a blank history and the fork point would be lost.
#[test]
fn test_fork_writes_runnable_checkpoint_for_new_session() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let history = vec![
        Message::user("Plan the migration"),
        Message::assistant("Step 1: inventory the call sites"),
    ];
    // Source checkpoint was an ApprovalRequired yield with pending state — the
    // fork must NOT inherit that; it should be a clean Hibernation checkpoint.
    let mut cp = test_checkpoint("source-session", &turn_id_for(3), history.clone(), 3);
    cp.yield_reason = YieldReason::ApprovalRequired {
        approval_request_id: "apr-xyz".to_string(),
    };
    cp.suspended_at = Some("2024-01-01T00:00:01Z".to_string());
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork_from_checkpoint(
        &config,
        &cp,
        Some("branch-session"),
        Some("Try approach B instead"),
    )
    .unwrap();
    assert_eq!(fork.new_session_id, "branch-session");

    // The forked session now has a checkpoint the engine can resume from.
    let forked_cp = load_latest_checkpoint(&config, "branch-session")
        .unwrap()
        .expect("fork must write a checkpoint under the new session id");

    assert_eq!(forked_cp.session_id, "branch-session");
    // Branch point preserved: full history + the branch message appended.
    assert_eq!(forked_cp.history.len(), 3);
    assert_eq!(forked_cp.history[2].content, "Try approach B instead");
    assert_eq!(forked_cp.turn_counter, 3);
    // Clean, auto-resumable yield — no inherited approval/pending state.
    assert!(matches!(forked_cp.yield_reason, YieldReason::Hibernation));
    assert!(forked_cp.pending_tool_state.is_none());
    assert!(forked_cp.suspended_at.is_none());
}

/// Test fork fails without any checkpoint.
#[test]
fn test_fork_fails_without_checkpoint() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let result = SessionFork::fork(&config, "nonexistent-session", None, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("No checkpoint found"));
}

/// Test fork_from_checkpoint with a specific checkpoint.
#[test]
fn test_fork_from_specific_checkpoint() {
    let temp = tempdir().unwrap();
    let config = test_config(&temp);

    let history = vec![Message::user("Hello"), Message::assistant("Hi!")];
    let cp = test_checkpoint("original-session", "turn-0002", history.clone(), 2);
    save_checkpoint(&config, &cp).unwrap();

    let fork = SessionFork::fork_from_checkpoint(
        &config,
        &cp,
        Some("checkpoint-fork"),
        Some("Continue from here"),
    )
    .unwrap();

    assert_eq!(fork.new_session_id, "checkpoint-fork");
    assert_eq!(fork.source_session_id, "original-session");
    assert_eq!(fork.fork_turn, 2);
    assert_eq!(fork.initial_history.len(), 3); // 2 original + 1 branch message
    assert_eq!(fork.initial_history[2].content, "Continue from here");
}
