//! `channel.bind` / `channel.resolve` JSON-RPC (#393, P3.c) — external channel
//! conversations bind to a room over the gateway API so channels are clients,
//! not direct store readers (#390).



use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::{Arc, OnceLock};
use crate::support::TestWorkspace;

struct SharedEnv {
    _ws: TestWorkspace,
    _store: Arc<GatewayStore>,
    router: JsonRpcRouter,
}

static SHARED: OnceLock<SharedEnv> = OnceLock::new();

// One shared workspace/router — initializing TestWorkspace concurrently from
// multiple tests races on global state, so all tests share a single env.
fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(ws.gateway_config(), Some(store.clone()));
        SharedEnv {
            _ws: ws,
            _store: store,
            router,
        }
    })
}

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "ch-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

#[tokio::test]
async fn channel_bind_then_resolve_round_trips() {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "channel.bind",
            serde_json::json!({
                "channel": "discord",
                "external_id": "thread-100",
                "root_session_id": "root-abc",
            }),
        ))
        .await;
    assert!(resp.error.is_none(), "bind errored: {:?}", resp.error);
    let bound = resp.result.expect("bind result");
    assert_eq!(bound["channel"], "discord");
    assert_eq!(bound["root_session_id"], "root-abc");
    let created_at = bound["created_at"].as_str().unwrap().to_string();

    // Resolve returns the same binding.
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "channel.resolve",
            serde_json::json!({ "channel": "discord", "external_id": "thread-100" }),
        ))
        .await;
    let result = resp.result.expect("resolve result");
    assert_eq!(result["binding"]["root_session_id"], "root-abc");

    // Rebinding the same conversation upserts root_session_id, preserves created_at.
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "channel.bind",
            serde_json::json!({
                "channel": "discord",
                "external_id": "thread-100",
                "root_session_id": "root-xyz",
            }),
        ))
        .await;
    let rebound = resp.result.expect("rebind result");
    assert_eq!(rebound["root_session_id"], "root-xyz");
    assert_eq!(
        rebound["created_at"].as_str().unwrap(),
        created_at,
        "rebind must preserve the original created_at"
    );
}

#[tokio::test]
async fn channel_resolve_unbound_returns_none() {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "channel.resolve",
            serde_json::json!({ "channel": "whatsapp", "external_id": "never-bound" }),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("resolve result");
    assert!(result["binding"].is_null(), "unbound must resolve to null");
}

#[tokio::test]
async fn channel_bind_requires_all_fields() {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "channel.bind",
            serde_json::json!({ "channel": "discord", "external_id": "  ", "root_session_id": "r" }),
        ))
        .await;
    let err = resp.error.expect("blank external_id must error");
    assert_eq!(err.code, -32602);
}
