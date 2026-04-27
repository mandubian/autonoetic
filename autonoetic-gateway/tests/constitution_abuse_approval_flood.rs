//! Constitution test: R+5 / R-7.17 — Approval flood cap.
//!
//! Verifies that pending approvals per root_session_id are bounded.
//! The N+1th request is rejected with `approval_flood`, and the first
//! N remain pending.

mod support;

use std::sync::Arc;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};
use autonoetic_types::config::GatewayConfig;

fn make_request(ix: usize, root_session_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: format!("req-{}", ix),
        agent_id: "test-agent".to_string(),
        session_id: format!("sess-{}", ix),
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: None,
        task_id: None,
        action: ScheduledAction::SandboxExec {
            command: format!("echo {}", ix),
            detected_hosts: Some(vec![format!("host-{}.example.com", ix)]),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
        },
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: Some(format!("test approval {}", ix)),
        evidence_ref: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
    }
}

#[serial_test::serial]
#[test]
fn flood_cap_rejects_at_limit_and_keeps_existing() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = GatewayStore::open(&gateway_dir)?;
    let cap: usize = 5;
    store.set_approval_flood_cap(cap);

    let root = "root-session-flood";

    // Insert exactly `cap` approvals — all should succeed.
    for i in 0..cap {
        let req = make_request(i, root);
        store.create_approval(&req)?;
    }

    // The cap+1th should be rejected.
    let over = make_request(cap, root);
    let result = store.create_approval(&over);
    assert!(result.is_err(), "approval beyond cap should be rejected");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("approval_flood"),
        "expected approval_flood error, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains(&format!("cap {}", cap)),
        "error should mention the cap, got: {}",
        err_msg
    );

    // The original cap approvals should still be pending.
    let pending = store.count_pending_for_root(root)?;
    assert_eq!(pending, cap, "all {} original approvals should remain pending", cap);

    // A different root session should not be affected.
    let other_root = "other-root-session";
    let other_req = make_request(999, other_root);
    store.create_approval(&other_req)?;
    assert_eq!(store.count_pending_for_root(other_root)?, 1);

    Ok(())
}

#[serial_test::serial]
#[test]
fn flood_cap_zero_means_disabled() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = GatewayStore::open(&gateway_dir)?;
    store.set_approval_flood_cap(0);

    let root = "root-no-cap";

    // Insert 60 approvals — all should succeed when cap = 0.
    for i in 0..60 {
        let req = make_request(i, root);
        store.create_approval(&req)?;
    }

    let pending = store.count_pending_for_root(root)?;
    assert_eq!(pending, 60);

    Ok(())
}

#[serial_test::serial]
#[test]
fn flood_cap_skipped_when_no_root_session_id() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = GatewayStore::open(&gateway_dir)?;
    store.set_approval_flood_cap(1);

    // Requests without root_session_id should bypass the cap.
    for i in 0..5 {
        let mut req = make_request(i, "unused");
        req.root_session_id = None;
        store.create_approval(&req)?;
    }

    Ok(())
}
