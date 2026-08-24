//! Escalation aggregation service layer (#1119 close-out) — the backend of
//! `admin.escalation_list` (pending + per-root stale), exercised at service
//! level (no second in-process router, per convention).

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, EscalationType};
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

fn escalation(id: &str, root: &str, status: EscalationStatus) -> EscalationMessage {
        let artifact_id = format!("art-{id}");
    EscalationMessage {
        escalation_id: id.to_string(),
        artifact_id,
        artifact_digest: None,
        agent_id: "coder.default".to_string(),
        revision_id: format!("rev-{id}"),
        role_verdicts: vec![],
        planner_synthesis: "review this".to_string(),
        created_at: "2026-08-24T10:00:00+00:00".to_string(),
        resolved_at: None,
        root_session_id: root.to_string(),
        status,
        decided_by: None,
        decision_reason: None,
        code_excerpts: None,
        escalation_type: EscalationType::default(),
        approval_request_id: None,
        expires_at: None,
    }
}

#[tokio::test]
async fn escalation_list_merges_pending_with_per_root_stale() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .create_escalation(&escalation(
            "esc-pending-a",
            "root-a",
            EscalationStatus::Pending,
        ))
        .expect("seed pending a");
    store
        .create_escalation(&escalation(
            "esc-pending-b",
            "root-a",
            EscalationStatus::Pending,
        ))
        .expect("seed pending b");
    store
        .create_escalation(&escalation(
            "esc-stale",
            "root-a",
            EscalationStatus::Stale,
        ))
        .expect("seed stale");

    let all = svc.escalations_with_stale().expect("aggregation");
    let ids: Vec<&str> = all.iter().map(|e| e.escalation_id.as_str()).collect();
    assert!(ids.contains(&"esc-pending-a"));
    assert!(ids.contains(&"esc-pending-b"));
    assert!(
        ids.contains(&"esc-stale"),
        "stale escalation must ride the per-root merge: {ids:?}"
    );

    // The RPC form ({escalations: [...]}) roundtrips the same messages.
    let raw = serde_json::to_value(all).expect("encode");
    let roundtrip: Vec<EscalationMessage> =
        serde_json::from_value(raw).expect("decode back to EscalationMessage");
    assert!(roundtrip.iter().any(|e| e.escalation_id == "esc-pending-a"));
}