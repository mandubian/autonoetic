//! `agent.suspend` / `agent.unsuspend` JSON-RPC operator surface (#514).
//!
//! These methods are what lets an operator actually invoke the suspension
//! lever; the enforcement (no new session, in-flight survives, read stays
//! open) is covered in `phase2_promotion_stability_integration`.


use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::AgentAliasRecord;
use autonoetic_types::principal::PrincipalKind;
use std::sync::{Arc, OnceLock};
use crate::support::TestWorkspace;

struct SharedEnv {
    _ws: TestWorkspace,
    store: Arc<GatewayStore>,
    router: JsonRpcRouter,
}

static SHARED: OnceLock<SharedEnv> = OnceLock::new();

// `JsonRpcRouter::new` initializes the process-global constitution runtime,
// which can only happen once per process — so all tests share one router.
fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(ws.gateway_config(), Some(store.clone()));
        SharedEnv {
            _ws: ws,
            store,
            router,
        }
    })
}

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "suspend-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

fn seed_alias(store: &GatewayStore, alias_id: &str) {
    store
        .upsert_agent_alias(&AgentAliasRecord {
            alias_id: alias_id.to_string(),
            agent_id: alias_id.to_string(),
            revision_id: "rev_seed".to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        })
        .unwrap();
}

#[tokio::test]
async fn agent_suspend_unsuspend_rpc_roundtrip() {
    let env = shared();
    let store = &env.store;
    let router = &env.router;
    let agent_id = "roundtrip.agent";
    seed_alias(store, agent_id);

    // Suspend → changes the row.
    let resp = router
        .dispatch(make_jsonrpc(
            "agent.suspend",
            serde_json::json!({ "agent_id": agent_id, "reason": "over-privileged", "suspended_by": "operator" }),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let r = resp.result.expect("result");
    assert_eq!(r["ok"], true);
    assert_eq!(r["suspended"], true);

    // Suspension is persisted with attribution.
    let alias = store.resolve_alias(agent_id).unwrap().expect("alias");
    assert!(alias.suspended_at.is_some());
    assert_eq!(alias.suspended_reason.as_deref(), Some("over-privileged"));
    assert_eq!(alias.suspended_by.as_deref(), Some("operator"));

    // Suspending again is a no-op (idempotent).
    let resp = router
        .dispatch(make_jsonrpc(
            "agent.suspend",
            serde_json::json!({ "agent_id": agent_id }),
        ))
        .await;
    assert_eq!(resp.result.expect("result")["suspended"], false);

    // Unsuspend → clears it.
    let resp = router
        .dispatch(make_jsonrpc(
            "agent.unsuspend",
            serde_json::json!({ "agent_id": agent_id }),
        ))
        .await;
    let r = resp.result.expect("result");
    assert_eq!(r["ok"], true);
    assert_eq!(r["unsuspended"], true);
    assert!(store.resolve_alias(agent_id).unwrap().unwrap().suspended_at.is_none());

    // Unsuspending again is a no-op.
    let resp = router
        .dispatch(make_jsonrpc(
            "agent.unsuspend",
            serde_json::json!({ "agent_id": agent_id }),
        ))
        .await;
    assert_eq!(resp.result.expect("result")["unsuspended"], false);
}

#[tokio::test]
async fn agent_suspend_unknown_agent_is_noop() {
    let env = shared();
    let router = &env.router;

    let resp = router
        .dispatch(make_jsonrpc(
            "agent.suspend",
            serde_json::json!({ "agent_id": "does.not.exist" }),
        ))
        .await;
    // No alias to suspend → ok, but nothing changed (not an error).
    let r = resp.result.expect("result");
    assert_eq!(r["ok"], true);
    assert_eq!(r["suspended"], false);
}
