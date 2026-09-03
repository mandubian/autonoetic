//! #606: checkpoint integrity violation handling.
//!
//! When a tampered checkpoint (HMAC mismatch) or an action-mismatch (TOCTOU
//! substitution) is detected on resume, the gateway must:
//!   1. emit a durable `background.checkpoint`/`checkpoint_tampered` causal event,
//!   2. revoke the bound approval with reason `integrity_violation`,
//!   3. surface an operator-visible alert.
//!
//! These tests exercise the store-level mechanisms (`find_latest_open_approval`,
//! `cancel_approval_for_integrity_violation`) and the
//! `record_checkpoint_integrity_violation` handler directly.

use autonoetic_gateway::execution::record_checkpoint_integrity_violation;
use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{
    checkpoints_dir, is_integrity_error, load_latest_checkpoint_strict, sanitize_path_component,
    save_checkpoint, turn_id_for, SessionCheckpoint, YieldReason,
};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn make_pending_approval(
    store: &GatewayStore,
    request_id: &str,
    session_id: &str,
) -> anyhow::Result<()> {
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
        action: ScheduledAction::SandboxExec {
            command: "echo hi".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            detected_mounts: None,
            intent: None,
        },
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

        expires_at: None,
    };
    store.create_approval(&mut request)?;
    Ok(())
}

fn default_checkpoint(session_id: &str, turn: u64) -> SessionCheckpoint {
    SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![Message::user("hello")],
        turn_counter: turn,
        loop_guard_state: LoopGuard::default(),
        session_state: autonoetic_types::agent::SessionState::Normal,
        tool_tier_escalated: false,
        session_phase: Default::default(),
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id_for(turn),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::ApprovalRequired {
            approval_request_id: "apr-tamper".to_string(),
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

/// HMAC tamper on the latest checkpoint is surfaced by the strict loader, the
/// handler emits the audit event, and the bound approval is revoked.
#[test]
fn hmac_tamper_on_resume_emits_event_and_revokes_approval() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = GatewayStore::open(&gateway_dir)?;
    let session_id = "root/coder-abc";

    // A pending approval bound to the session, plus a signed checkpoint.
    make_pending_approval(&store, "apr-tamper", session_id)?;
    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: temp.path().to_path_buf(),
        ..Default::default()
    };
    save_checkpoint(&config, &default_checkpoint(session_id, 1))?;

    // Attacker tampers the checkpoint file (break the HMAC).
    let cp_path = checkpoints_dir(&config)
        .join(sanitize_path_component(session_id))
        .join(format!("{}.checkpoint.json", turn_id_for(1)));
    assert!(
        cp_path.exists(),
        "checkpoint file should exist: {:?}",
        cp_path
    );
    let original = std::fs::read_to_string(&cp_path)?;
    let mut envelope: serde_json::Value = serde_json::from_str(&original)?;
    envelope["hmac_hex"] = serde_json::json!("deadbeef00".repeat(8));
    std::fs::write(&cp_path, serde_json::to_string(&envelope)?)?;

    // Strict loader surfaces the tamper.
    let err = load_latest_checkpoint_strict(&config, session_id)
        .expect_err("tampered checkpoint should surface an integrity error");
    assert!(is_integrity_error(&err));

    // Handler records the violation and revokes the approval (located by
    // session since the checkpoint is unreadable).
    record_checkpoint_integrity_violation(
        &store,
        session_id,
        "test-agent",
        None,
        "checkpoint HMAC verification failed on resume",
    );

    // The causal event must be present.
    let events = store.search_causal_events(Some(session_id), None, 50)?;
    let tamper = events
        .iter()
        .find(|e| e.action == "checkpoint_tampered")
        .expect("checkpoint_tampered causal event should be emitted");
    assert_eq!(tamper.status, "integrity_violation");
    assert_eq!(tamper.target.as_deref(), Some("apr-tamper"));

    // The approval must be cancelled.
    let approval = store
        .get_approval("apr-tamper")?
        .expect("approval should exist");
    assert_eq!(approval.status, Some(ApprovalStatus::Cancelled));
    assert_eq!(
        approval.decision_reason.as_deref(),
        Some("integrity_violation")
    );

    Ok(())
}

/// Action-mismatch (TOCTOU): the checkpoint loaded fine but the bound action
/// differs. The handler is called with the explicit approval id and revokes an
/// already-approved row.
#[test]
fn action_mismatch_revokes_already_approved_approval() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let session_id = "root/coder-xyz";
    make_pending_approval(&store, "apr-mismatch", session_id)?;

    // Simulate the operator approving it, then a substitution being detected.
    store.record_decision(
        "apr-mismatch",
        "approved",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("approved"),
    )?;

    record_checkpoint_integrity_violation(
        &store,
        session_id,
        "test-agent",
        Some("apr-mismatch"),
        "checkpoint action mismatch — possible substitution attack",
    );

    // The already-approved row must be force-cancelled.
    let approval = store
        .get_approval("apr-mismatch")?
        .expect("approval exists");
    assert_eq!(
        approval.status,
        Some(ApprovalStatus::Cancelled),
        "action-mismatch should force-cancel an approved row"
    );
    assert_eq!(
        approval.decision_reason.as_deref(),
        Some("integrity_violation")
    );

    let events = store.search_causal_events(Some(session_id), None, 50)?;
    assert!(
        events
            .iter()
            .any(|e| e.action == "checkpoint_tampered"
                && e.target.as_deref() == Some("apr-mismatch")),
        "checkpoint_tampered event must reference the approval id"
    );

    Ok(())
}

/// `cancel_approval_for_integrity_violation` leaves already-terminal
/// (rejected) approvals untouched and reports no-op.
#[test]
fn integrity_cancel_leaves_rejected_approvals_untouched() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    make_pending_approval(&store, "apr-rejected", "root/sess")?;
    store.record_decision(
        "apr-rejected",
        "rejected",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("no"),
    )?;

    let changed = store.cancel_approval_for_integrity_violation("apr-rejected")?;
    assert!(!changed, "already-rejected approval should not be touched");

    let approval = store.get_approval("apr-rejected")?.unwrap();
    assert_eq!(approval.status, Some(ApprovalStatus::Rejected));

    Ok(())
}
