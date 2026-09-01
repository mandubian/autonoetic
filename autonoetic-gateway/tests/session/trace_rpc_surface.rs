//! `trace.*` RPC service layer (#1119 tranche 6) — the surface behind
//! `autonoetic trace contract-health|civic-health|session|fork-tree`
//! and the workflow interaction polls, exercised at service level (no second
//! in-process router, per the established pattern).

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::causal_chain::CausalEventRecord;
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

fn causal_event(session_id: &str, action: &str, status: &str) -> CausalEventRecord {
    CausalEventRecord {
        event_id: format!("ev-{}-{action}", uuid::Uuid::new_v4()),
        agent_id: "coder.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: Some("turn-1".to_string()),
        event_seq: 1,
        timestamp: "2026-08-24T10:00:00+00:00".to_string(),
        category: "tool".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: vec![],
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    }
}

#[tokio::test]
async fn causal_search_filters_by_session_and_obeys_limit() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let _ = store.create_causal_event(&causal_event("tr-sess-a", "content.read", "SUCCESS"));
    let _ = store.create_causal_event(&causal_event("tr-sess-a", "web.search", "SUCCESS"));
    let _ = store.create_causal_event(&causal_event("tr-sess-b", "agent.spawn", "SUCCESS"));

    let a = svc.causal_search(Some("tr-sess-a"), None, 100).expect("search");
    let a = a.as_array().expect("array");
    assert_eq!(a.len(), 2, "two events for session a: {a:?}");
    assert!(a.iter().all(|e| e["session_id"] == "tr-sess-a"));

    let limited = svc.causal_search(Some("tr-sess-a"), None, 1).expect("limited");
    assert_eq!(limited.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn contract_health_tallies_attributed_events() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    // One event attributed to a registered rule id (P-5.2 → clause P-5)
    // and one unattributed (a synthetic rule id not in the register).
    let mut attributed = causal_event("tr-health", "approval.flood", "DENIED");
    attributed.enforced_rules = vec!["P-5.2".to_string()];
    let _ = store.create_causal_event(&attributed);
    let mut unattributed = causal_event("tr-health", "mystery.action", "DENIED");
    unattributed.enforced_rules = vec!["P-X.99".to_string()];
    let _ = store.create_causal_event(&unattributed);

    let health = svc.contract_health(None).expect("contract health");
    let by_clause = health["by_clause"].as_array().expect("by_clause");
    assert!(
        by_clause.iter().any(|e| e["clause"] == "P-5"),
        "attributed clause tallied: {by_clause:?}"
    );
    assert!(health["unattributed"].as_u64().unwrap_or(0) >= 1);
    assert!(health["registered_clause_count"].as_u64().unwrap_or(0) > 0);
    let p52 = by_clause.iter().find(|e| e["clause"] == "P-5").expect("P-5.2 tallied");
    assert!(p52["title"].as_str().is_some());
}

// Note: civic_health needs constitutional proposals / anomaly flags seeded;
// the tallies reuse the store queries covered elsewhere, so this pins the
// aggregation shape only when data exists (empty is a valid answer).
#[tokio::test]
async fn civic_health_returns_agent_rows_or_empty() {
    let health = service().civic_health(None).expect("civic health");
    assert!(health["by_agent"].is_array());
}

#[tokio::test]
async fn fork_tree_roundtrips_seeded_lineage() {
    use autonoetic_gateway::runtime::checkpoint::SessionFork;

    let svc = service();
    let store = svc.gateway_store().expect("store");
    // Seed a fork lineage row directly (forking checkpoints needs agent dirs).
    let fork = SessionFork {
        source_session_id: "tr-fork-root".to_string(),
        new_session_id: "tr-fork-child".to_string(),
        fork_turn: 3,
        initial_history: vec![],
        history_handle: "hh".to_string(),
        agent_id: "coder.default".to_string(),
    };
    store
        .record_session_fork(&fork, Some("branch"), "coder.default")
        .expect("record fork");

    let tree = svc.fork_tree("tr-fork-root").expect("fork tree");
    assert_eq!(tree["root_session_id"].as_str(), Some("tr-fork-root"));
    let descendants = tree["descendants"].as_array().expect("descendants");
    assert_eq!(descendants.len(), 1, "{tree}");
    assert_eq!(descendants[0]["forked_session_id"].as_str(), Some("tr-fork-child"));
    assert_eq!(descendants[0]["fork_turn"].as_u64(), Some(3));
    assert!(descendants[0]["children"].as_array().expect("children").is_empty());

    // Child side: ancestor chain includes the parent.
    let tree = svc.fork_tree("tr-fork-child").expect("child tree");
    let ancestors = tree["ancestors"].as_array().expect("ancestors");
    assert_eq!(ancestors.len(), 1, "{tree}");
    assert_eq!(ancestors[0]["source_session_id"].as_str(), Some("tr-fork-root"));
}

#[tokio::test]
async fn fork_tree_cycle_does_not_reintroduce_the_root() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    // A self-referencing lineage row (child -> root) must not re-introduce
    // the root inside its own descendant tree (visited set is seeded with
    // the target root).
    let fork = autonoetic_gateway::runtime::checkpoint::SessionFork {
        source_session_id: "tr-cycle-root".to_string(),
        new_session_id: "tr-cycle-root".to_string(),
        fork_turn: 1,
        initial_history: vec![],
        history_handle: "hh".to_string(),
        agent_id: "coder.default".to_string(),
    };
    let _ = store.record_session_fork(&fork, None, "coder.default");

    let tree = svc.fork_tree("tr-cycle-root").expect("fork tree");
    // Descendants must not contain the root itself despite the cycle.
    fn collect_ids(nodes: &[serde_json::Value], out: &mut Vec<String>) {
        for n in nodes {
            out.push(n["forked_session_id"].as_str().unwrap_or("?").to_string());
            collect_ids(
                n["children"].as_array().map(|a| a.as_slice()).unwrap_or(&[]),
                out,
            );
        }
    }
    let mut ids = Vec::new();
    collect_ids(tree["descendants"].as_array().expect("descendants"), &mut ids);
    assert!(
        !ids.contains(&"tr-cycle-root".to_string()),
        "root re-introduced as its own descendant: {ids:?}"
    );
}