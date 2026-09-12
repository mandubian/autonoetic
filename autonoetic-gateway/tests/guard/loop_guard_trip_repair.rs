//! LoopGuard trip → suspend → repair contract tests.
//!
//! A LoopGuard trip used to yield `MaxTurnsReached`, which is not
//! auto-resumable — so a root planner that tripped mid-workflow closed as
//! `spawn_execute_error` and cascade-failed the whole workflow
//! (`fail_workflow_for_root_session`), including in-flight async children.
//! The child then hit the terminal workflow on its next `agent_spawn` and
//! hard-tripped `WorkflowTerminal` in turn.
//!
//! The fix: trips yield `YieldReason::LoopGuardTripped { reason_code,
//! repairable, repairs }`. Repairable trip classes stay auto-resumable
//! (bounded by `MAX_LOOP_GUARD_REPAIRS`) so the session closes as suspended,
//! children keep running, and the next signal-driven resume repairs the
//! guard (`clear_trip_for_repair`). These tests pin:
//!
//! - The `LoopGuardTripped` yield-reason shape survives a real checkpoint
//!   save/load cycle (the persisted contract every reader depends on).
//! - Repairability classification of trip reasons (behavioral vs budget).
//! - The repair cycle at guard level: trip → repair → healthy → trip again,
//!   with the repair count accumulating and failure budgets untouched.
//!
//! What this file does NOT cover: the lifecycle/execution wiring that saves
//! the yield reason on `check_loop` failure and applies the repair inside
//! `SessionCheckpoint::restore_into` — that needs a stub-LLM-driven
//! `AgentExecutor` turn (same limitation as `spawn_identity_loop_guard.rs`).

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    load_latest_checkpoint, save_checkpoint, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::{LoopGuard, LoopGuardTripReason, MAX_LOOP_GUARD_REPAIRS};
use autonoetic_types::config::GatewayConfig;

fn test_config(temp: &tempfile::TempDir) -> GatewayConfig {
    GatewayConfig {
        runtime_dir: temp.path().to_path_buf().join(".gateway"),
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    }
}

fn checkpoint_with_yield_reason(
    session_id: &str,
    yield_reason: YieldReason,
) -> SessionCheckpoint {
    SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![
            Message::system("You are a test agent"),
            Message::user("Hello, test"),
        ],
        turn_counter: 5,
        session_state: Default::default(),
        tool_tier_escalated: false,
        session_phase: Default::default(),
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        loop_guard_state: LoopGuard::default(),
        agent_id: "planner.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-005".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason,
        content_store_refs: vec![],
        created_at: "2026-09-11T00:00:00Z".to_string(),
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
        feedback_events: vec![],
    }
}

/// The persisted contract: a checkpoint saved by a LoopGuard trip carries the
/// trip's identity (reason code, repairability, repair count) through a real
/// save/load cycle, so the resume path can gate auto-resume and apply the
/// repair without re-deriving anything.
#[test]
fn loop_guard_tripped_checkpoint_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let config = test_config(&temp);
    let session_id = "session-loopguard-roundtrip";

    let cp = checkpoint_with_yield_reason(
        session_id,
        YieldReason::LoopGuardTripped {
            reason_code: "no_meaningful_progress".to_string(),
            repairable: true,
            repairs: 1,
        },
    );
    save_checkpoint(&config, &cp).expect("save tripped checkpoint");

    let loaded = load_latest_checkpoint(&config, session_id)
        .expect("load tripped checkpoint")
        .expect("checkpoint exists");

    match &loaded.yield_reason {
        YieldReason::LoopGuardTripped {
            reason_code,
            repairable,
            repairs,
        } => {
            assert_eq!(reason_code, "no_meaningful_progress");
            assert!(*repairable);
            assert_eq!(*repairs, 1);
        }
        other => panic!("expected LoopGuardTripped, got {other:?}"),
    }
    // The checkpointed guard state round-trips too — the resumed executor
    // restores counters from it before the repair is applied.
    assert_eq!(loaded.loop_guard_state.loop_guard_repairs_used, 0);
}

/// Legacy checkpoints (pre-fix trips yielded `MaxTurnsReached`) must still
/// load — the new variant is additive and the old reason keeps its
/// non-resumable semantics.
#[test]
fn legacy_max_turns_reached_checkpoint_still_loads() {
    let temp = tempfile::tempdir().unwrap();
    let config = test_config(&temp);
    let session_id = "session-loopguard-legacy";

    let cp = checkpoint_with_yield_reason(session_id, YieldReason::MaxTurnsReached);
    save_checkpoint(&config, &cp).expect("save legacy checkpoint");

    let loaded = load_latest_checkpoint(&config, session_id)
        .expect("load legacy checkpoint")
        .expect("checkpoint exists");
    assert!(matches!(loaded.yield_reason, YieldReason::MaxTurnsReached));
}

/// The published repair budget: `MAX_LOOP_GUARD_REPAIRS` must stay small —
/// it bounds how many times a spinning session can be resurrected before the
/// operator must intervene. Pinned so a silent default bump surfaces here.
#[test]
fn repair_budget_is_small_by_construction() {
    assert!(
        MAX_LOOP_GUARD_REPAIRS >= 1,
        "a single repair must always be available: the first trip's close must be a suspension, not a workflow-failing error"
    );
    assert!(
        MAX_LOOP_GUARD_REPAIRS <= 5,
        "the repair budget is a spin bound, not a retry allowance — keep it tight"
    );
}

/// Repairability classification: the public contract the yield-reason
/// `repairable` flag is built from.
#[test]
fn trip_reason_repairability_classification() {
    // Behavioral: strategy-level spin shapes.
    assert!(LoopGuardTripReason::NoMeaningfulProgress { cycles: 10 }.is_session_repairable());
    assert!(
        LoopGuardTripReason::RedundantRosterPolling {
            tool: "agent_list".to_string(),
            repeats: 3,
            floor: 3,
        }
        .is_session_repairable()
    );
    // Deterministic / budget: resume would re-trip instantly or paper over a
    // standing problem.
    assert!(
        !LoopGuardTripReason::WorkflowTerminal {
            workflow_id: "wf-x".to_string()
        }
        .is_session_repairable()
    );
    assert!(
        !LoopGuardTripReason::ChildFailureBudget { failures: 5 }.is_session_repairable()
    );
}

/// The repair cycle at guard level: trip → repair → healthy → trip again.
/// Failure budgets deliberately survive repairs (non-repairable class), and
/// each repair spends one unit of the persisted budget.
#[test]
fn trip_repair_cycle_accumulates_repairs_and_keeps_budgets() {
    let mut guard = LoopGuard::new(2);

    // First spin: 2 cycles then trip.
    guard.check_loop().expect("cycle 1");
    guard.check_loop().expect("cycle 2");
    guard.check_loop().expect_err("no-progress trip");

    guard.clear_trip_for_repair();
    assert_eq!(guard.loop_guard_repairs_used, 1);
    assert!(guard.last_trip_reason().is_none(), "trip latched cleared");

    // Second spin trips again; a second repair is still allowed — the
    // auto-resume gate compares `repairs < MAX_LOOP_GUARD_REPAIRS`.
    guard.check_loop().expect("cycle 1 after repair");
    guard.check_loop().expect("cycle 2 after repair");
    guard.check_loop().expect_err("second no-progress trip");
    guard.clear_trip_for_repair();
    assert_eq!(guard.loop_guard_repairs_used, 2);

    // Per-tool failure budgets survive repairs by design: they are totals
    // that catch alternating-failure patterns, and a repaired session must
    // not get its failure history wiped.
    guard.register_failure("sandbox_exec", "", None);
    guard.clear_trip_for_repair_if_tripped_for_test();
    assert_eq!(guard.tool_failure_counts.get("sandbox_exec"), Some(&1));
}

/// Helper local to this test: repair only if the guard has actually tripped,
/// mirroring `restore_into`'s repairable-trip gate.
trait ClearIfTripped {
    fn clear_trip_for_repair_if_tripped_for_test(&mut self);
}

impl ClearIfTripped for LoopGuard {
    fn clear_trip_for_repair_if_tripped_for_test(&mut self) {
        if self.last_trip_reason().is_some() {
            self.clear_trip_for_repair();
        }
    }
}
