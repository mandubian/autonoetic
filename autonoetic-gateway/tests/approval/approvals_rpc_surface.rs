//! `approvals.list` / `approvals.stats` service layer (#1119 tranche 7) —
//! the RPC surface behind the CLI approvals surface (headless + TUI + wiki),
//! exercised at service level (no second in-process router, per convention).

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ScheduledAction,
};
use std::sync::Arc;

fn service() -> &'static GatewayExecutionService {
    static SERVICE: std::sync::OnceLock<GatewayExecutionService> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| {
        let ws = tempfile::tempdir().expect("tempdir");
        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: ws.path().join("agents"),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        std::mem::forget(ws);
        GatewayExecutionService::new(config, Some(store))
    })
}

fn seed_approval(store: &std::sync::Arc<GatewayStore>, request_id: &str) {
    let mut approval = ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: format!("{}-session", request_id),
        action: ScheduledAction::SandboxExec {
            command: format!("run-{}", request_id),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: "2026-08-24T10:00:00+00:00".to_string(),
        reason: Some("test approval".to_string()),
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some(format!("{}-session", request_id)),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut approval).expect("seed approval");
}

#[tokio::test]
async fn pending_approvals_lists_seeded_and_roundtrips_action() {
    // The store is shared across tests in this binary (static service), so
    // assert containment, not exact counts.
    let svc = service();
    let store = svc.gateway_store().expect("store");
    seed_approval(&store, "apr-rpc-list");

    let pending = svc.pending_approvals().expect("list");
    let mine = pending
        .iter()
        .find(|a| a.request_id == "apr-rpc-list")
        .expect("seeded approval listed");
    assert_eq!(mine.agent_id, "coder.default");
    assert_eq!(
        mine.action,
        ScheduledAction::SandboxExec {
            command: "run-apr-rpc-list".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
        }
    );
}

#[tokio::test]
async fn approval_stats_reports_totals() {
    // Seed here too: tests run concurrently against the shared service.
    let svc = service();
    let store = svc.gateway_store().expect("store");
    seed_approval(&store, "apr-rpc-stats");
    let stats = svc.approval_stats(None, None, None).expect("stats");
    assert!(stats["total"].as_i64().unwrap_or(0) >= 1, "{stats}");
}