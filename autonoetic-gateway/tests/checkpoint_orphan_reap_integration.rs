//! #607: reap orphan checkpoint files on approval reject/withdraw + startup.
//!
//! When an approval is rejected or cancelled the suspended turn is dead, so
//! its signed checkpoint file must be reaped (not leaked on disk). A startup
//! reaper clears orphans left behind by a crash during reject/cancel.

use autonoetic_gateway::runtime::checkpoint::{
    checkpoints_dir, reap_orphan_checkpoints, sanitize_path_component, save_checkpoint,
    turn_id_for, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::scheduler::approval::{apply_decision, ApproveOptions, DecisionContext};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::llm::Message;
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn sandbox_action() -> ScheduledAction {
    ScheduledAction::SandboxExec {
        command: "echo hi".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: None,
        intent: None,
    }
}

fn make_approval(store: &GatewayStore, request_id: &str, session_id: &str) {
    let mut request = ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(
            session_id
                .split('/')
                .next()
                .unwrap_or(session_id)
                .to_string(),
        ),
        workflow_id: None,
        task_id: None,
        action: sandbox_action(),
        created_at: chrono::Utc::now().to_rfc3339(),
        status: None,
        decided_at: None,
        decided_by: None,
        reason: Some("test".to_string()),
        evidence_ref: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    };
    store.create_approval(&mut request).unwrap();
}

fn bound_checkpoint(session_id: &str, turn: u64, approval_id: &str) -> SessionCheckpoint {
    SessionCheckpoint {
        history: vec![Message::user("hello")],
        turn_counter: turn,
        loop_guard_state: LoopGuard::default(),
        session_state: autonoetic_types::agent::SessionState::Normal,
        tool_tier_escalated: false,
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id_for(turn),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::ApprovalRequired {
            approval_request_id: approval_id.to_string(),
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

fn cp_path(config: &GatewayConfig, session_id: &str, turn: u64) -> std::path::PathBuf {
    checkpoints_dir(config)
        .join(sanitize_path_component(session_id))
        .join(format!("{}.checkpoint.json", turn_id_for(turn)))
}

fn apply(
    config: &GatewayConfig,
    store: &GatewayStore,
    request_id: &str,
    session_id: &str,
    status: ApprovalStatus,
) {
    let decision = ApprovalDecision {
        request_id: request_id.to_string(),
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        action: sandbox_action(),
        status,
        decided_at: chrono::Utc::now().to_rfc3339(),
        decided_by: "operator".to_string(),
        reason: Some("test".to_string()),
        root_session_id: Some(session_id.to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: ApprovalLevel::Operator,
    };
    apply_decision(
        config,
        Some(store),
        &decision,
        &ApproveOptions::default(),
        &DecisionContext {
            wiki_materialized_meta: None,
            hook_executor: None,
        },
    )
    .unwrap();
}

#[test]
fn reject_reaps_bound_checkpoint() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    };

    let session_id = "root/sess-reject";
    make_approval(&store, "apr-reject", session_id);
    save_checkpoint(&config, &bound_checkpoint(session_id, 1, "apr-reject"))?;
    assert!(cp_path(&config, session_id, 1).exists());

    apply(&config, &store, "apr-reject", session_id, ApprovalStatus::Rejected);

    assert!(
        !cp_path(&config, session_id, 1).exists(),
        "checkpoint must be reaped after reject"
    );
    Ok(())
}

#[test]
fn cancel_reaps_bound_checkpoint() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    };

    let session_id = "root/sess-cancel";
    make_approval(&store, "apr-cancel", session_id);
    save_checkpoint(&config, &bound_checkpoint(session_id, 2, "apr-cancel"))?;

    apply(&config, &store, "apr-cancel", session_id, ApprovalStatus::Cancelled);

    assert!(
        !cp_path(&config, session_id, 2).exists(),
        "checkpoint must be reaped after cancel"
    );
    Ok(())
}

#[test]
fn approved_keeps_checkpoint_for_resume() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    };

    let session_id = "root/sess-approve";
    make_approval(&store, "apr-approve", session_id);
    save_checkpoint(&config, &bound_checkpoint(session_id, 1, "apr-approve"))?;

    apply(&config, &store, "apr-approve", session_id, ApprovalStatus::Approved);

    assert!(
        cp_path(&config, session_id, 1).exists(),
        "approved approval must keep its checkpoint for resume"
    );
    Ok(())
}

#[test]
fn startup_reaper_clears_orphans_but_keeps_active() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;
    let config = GatewayConfig {
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    };

    // (a) Orphan: rejected approval + checkpoint.
    let orphan_session = "root/orphan";
    make_approval(&store, "apr-orphan", orphan_session);
    store.record_decision(
        "apr-orphan",
        "rejected",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("no"),
    )?;
    save_checkpoint(&config, &bound_checkpoint(orphan_session, 1, "apr-orphan"))?;
    assert!(cp_path(&config, orphan_session, 1).exists());

    // (b) Orphan: approval row missing entirely + checkpoint.
    let missing_session = "root/missing";
    save_checkpoint(&config, &bound_checkpoint(missing_session, 1, "apr-gone"))?;
    assert!(cp_path(&config, missing_session, 1).exists());

    // (c) Active: still-pending approval + checkpoint — must survive.
    let active_session = "root/active";
    make_approval(&store, "apr-active", active_session);
    save_checkpoint(&config, &bound_checkpoint(active_session, 1, "apr-active"))?;

    // (d) Approved (will resume) + checkpoint — must survive.
    let approved_session = "root/approved";
    make_approval(&store, "apr-appr", approved_session);
    store.record_decision(
        "apr-appr",
        "approved",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("yes"),
    )?;
    save_checkpoint(&config, &bound_checkpoint(approved_session, 1, "apr-appr"))?;

    let reaped = reap_orphan_checkpoints(&config, &store)?;
    assert_eq!(reaped, 2, "reaper should clear the rejected + missing orphans");

    assert!(
        !cp_path(&config, orphan_session, 1).exists(),
        "rejected-orphan checkpoint must be reaped"
    );
    assert!(
        !cp_path(&config, missing_session, 1).exists(),
        "missing-approval orphan checkpoint must be reaped"
    );
    assert!(
        cp_path(&config, active_session, 1).exists(),
        "pending-approval checkpoint must survive the reaper"
    );
    assert!(
        cp_path(&config, approved_session, 1).exists(),
        "approved (will-resume) checkpoint must survive the reaper"
    );
    Ok(())
}
