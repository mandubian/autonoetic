//! `constitution.list_proposals` / `proposal_get` / `proposal_decide`
//! service layer (#1119 tranche 5) — the RPC surface behind
//! `autonoetic gateway constitution proposals` list/show/decide, exercised
//! at service level (no second in-process router, per the established
//! pattern).

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::{constitutional_proposals::ConstitutionalProposal, GatewayStore};
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

fn seed_proposal(proposal_id: &str, status: &str, proposer: &str) -> ConstitutionalProposal {
    ConstitutionalProposal {
        proposal_id: proposal_id.to_string(),
        kind: "rights_amendment".to_string(),
        status: status.to_string(),
        proposer_agent_id: proposer.to_string(),
        proposer_session_id: None,
        justification: format!("{proposal_id} justification"),
        proposed_text: Some("New Ri-0.99".to_string()),
        evidence_json: serde_json::json!({"anchor": "causal-ev-1"}),
        target_id: None,
        created_at: "2026-08-24T10:00:00+00:00".to_string(),
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        published_in_release: None,
        operator_decision: None,
        sla_breached_at: None,
    }
}

#[tokio::test]
async fn list_filters_by_status_and_proposer() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .insert_constitutional_proposal(&seed_proposal("prop-a", "pending", "auditor.default"))
        .expect("seed a");
    store
        .insert_constitutional_proposal(&seed_proposal("prop-b", "approved", "planner.default"))
        .expect("seed b");

    let pending = svc.constitutional_proposals(Some("pending"), None, 50).expect("pending");
    let arr = pending.as_array().expect("array");
    assert!(arr.iter().any(|p| p["proposal_id"] == "prop-a"));
    assert!(!arr.iter().any(|p| p["proposal_id"] == "prop-b"));

    // The store is shared across tests in this binary (static service), so
    // assert containment by proposer, not exact counts.
    let auditor = svc.constitutional_proposals(None, Some("auditor.default"), 50).expect("by proposer");
    let arr = auditor.as_array().expect("array");
    assert!(arr.iter().any(|p| p["proposal_id"] == "prop-a"));
    assert!(!arr.iter().any(|p| p["proposal_id"] == "prop-b"), "planner's proposal must not match the auditor filter");

    let all = svc.constitutional_proposals(None, None, 50).expect("all");
    let all_arr = all.as_array().expect("array");
    assert!(all_arr.iter().any(|p| p["proposal_id"] == "prop-a"));
    assert!(all_arr.iter().any(|p| p["proposal_id"] == "prop-b"));
}

#[tokio::test]
async fn proposal_get_roundtrips_and_missing_is_an_error() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .insert_constitutional_proposal(&seed_proposal("prop-show", "pending", "auditor.default"))
        .expect("seed");

    let got = svc.constitutional_proposal("prop-show").expect("get");
    assert_eq!(got["proposal_id"].as_str(), Some("prop-show"));
    assert_eq!(got["kind"].as_str(), Some("rights_amendment"));

    let err = svc
        .constitutional_proposal("prop-missing")
        .expect_err("missing must error");
    assert!(err.to_string().contains("No proposal"), "{err}");
}

#[tokio::test]
async fn decide_updates_status_and_records_operator() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .insert_constitutional_proposal(&seed_proposal("prop-decide", "pending", "auditor.default"))
        .expect("seed");

    svc.decide_constitutional_proposal("prop-decide", "approved", Some("reviewed"))
        .expect("decide");

    let got = svc.constitutional_proposal("prop-decide").expect("get");
    assert_eq!(got["status"].as_str(), Some("approved"));
    assert_eq!(got["decided_by"].as_str(), Some("operator"));
    assert_eq!(got["decision_reason"].as_str(), Some("reviewed"));
    assert!(got["decided_at"].as_str().is_some());

    let err = svc
        .decide_constitutional_proposal("prop-missing", "approved", None)
        .expect_err("missing must error");
    assert!(err.to_string().contains("No proposal"), "{err}");
}