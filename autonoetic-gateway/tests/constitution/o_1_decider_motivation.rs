//! Constitution §O / O-1 — decider motivation obligation (#359 / #395).
//!
//! A principal decider's rejection without a motivation is refused at the public
//! approval-decision path, and the gate stays pending (no decision committed).
//! This pins the §O clause to its enforcement (`enforce_decider_motivation`).

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::config::{DeciderObligationsConfig, GatewayConfig};
use tempfile::tempdir;

fn pending_sandbox_approval(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: "root-session/coder-1".to_string(),
        action: ScheduledAction::SandboxExec {
            command: "echo hi".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        },
        created_at: "2026-06-01T00:00:00Z".to_string(),
        reason: None,
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    }
}

fn config_with_obligations(agents_dir: std::path::PathBuf, enabled: bool) -> GatewayConfig {
    GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        decider_obligations: DeciderObligationsConfig {
            enabled,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn o_1_principal_rejection_without_motivation_is_refused() {
    let dir = tempdir().unwrap();
    let agents_dir = dir.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let cfg = config_with_obligations(agents_dir, true);
    let store = GatewayStore::open(&gateway_dir).unwrap();

    let mut req = pending_sandbox_approval("apr-o1");
    store.create_approval(&mut req).unwrap();

    // Operator rejection with no motivation → refused (§O / O-1, mirror of Ri-0.3).
    let err = autonoetic_gateway::scheduler::reject_request(
        &cfg,
        Some(&store),
        "apr-o1",
        "operator",
        None,
        None,
    )
    .expect_err("a motivation-less rejection must be refused");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("§o") || msg.contains("motivation"),
        "expected a §O motivation refusal, got: {err}"
    );

    // The refusal must NOT have committed a decision — the gate is still pending.
    let still = store.get_approval("apr-o1").unwrap().unwrap();
    assert!(
        still.status.is_none(),
        "a refused decision must leave the approval pending"
    );
}

#[test]
fn o_1_disabled_config_allows_unmotivated_rejection() {
    let dir = tempdir().unwrap();
    let agents_dir = dir.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let cfg = config_with_obligations(agents_dir, false);
    let store = GatewayStore::open(&gateway_dir).unwrap();

    let mut req = pending_sandbox_approval("apr-o1-off");
    store.create_approval(&mut req).unwrap();

    // With enforcement disabled, the §O check does not fire (any downstream
    // result is acceptable — we only assert it isn't the §O refusal).
    if let Err(e) = autonoetic_gateway::scheduler::reject_request(
        &cfg,
        Some(&store),
        "apr-o1-off",
        "operator",
        None,
        None,
    ) {
        let msg = e.to_string().to_lowercase();
        assert!(
            !msg.contains("§o") && !msg.contains("motivation"),
            "§O must not fire when disabled, got: {e}"
        );
    }
}
