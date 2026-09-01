
use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::{Arc, OnceLock};
use crate::support::TestWorkspace;

struct SharedEnv {
    _ws: TestWorkspace,
    _store: Arc<GatewayStore>,
    router: JsonRpcRouter,
    router_no_store: JsonRpcRouter,
}

static SHARED: OnceLock<SharedEnv> = OnceLock::new();

fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let config = ws.gateway_config();
        let router = JsonRpcRouter::new(config.clone(), Some(store.clone()));
        let router_no_store = JsonRpcRouter::new(config, None);
        SharedEnv {
            _ws: ws,
            _store: store,
            router,
            router_no_store,
        }
    })
}

fn make_jsonrpc(id: &str, method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: id.to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

#[tokio::test]
async fn gate_add_and_get_roundtrip() {
    let env = shared();

    let add_resp = env
        .router
        .dispatch(make_jsonrpc(
            "add1",
            "gate.add_message",
            serde_json::json!({
                "gate_id": "apr-roundtrip-test",
                "sender": "operator",
                "content": "Why does the agent need localhost?",
            }),
        ))
        .await;
    assert!(add_resp.error.is_none(), "add should succeed: {:?}", add_resp.error);
    let msg_id = add_resp.result.expect("add result")["message_id"]
        .as_i64()
        .expect("message_id");
    assert!(msg_id > 0);

    let get_resp = env
        .router
        .dispatch(make_jsonrpc(
            "get1",
            "gate.get_messages",
            serde_json::json!({ "gate_id": "apr-roundtrip-test" }),
        ))
        .await;
    assert!(get_resp.error.is_none(), "get should succeed: {:?}", get_resp.error);
    let result = get_resp.result.expect("get result");
    let messages = result["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["sender"].as_str(), Some("operator"));
    assert_eq!(
        messages[0]["content"].as_str(),
        Some("Why does the agent need localhost?")
    );
}

#[tokio::test]
async fn gate_add_rejects_empty_gate_id() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "e1",
            "gate.add_message",
            serde_json::json!({
                "gate_id": "  ",
                "sender": "operator",
                "content": "test",
            }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("gate_id must not be empty"));
}

#[tokio::test]
async fn gate_add_rejects_empty_content() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "e2",
            "gate.add_message",
            serde_json::json!({
                "gate_id": "test-id",
                "sender": "operator",
                "content": "",
            }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("content must not be empty"));
}

#[tokio::test]
async fn gate_add_rejects_invalid_sender() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "e3",
            "gate.add_message",
            serde_json::json!({
                "gate_id": "test-id",
                "sender": "hacker",
                "content": "hello",
            }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("sender must be one of"));
}

#[tokio::test]
async fn gate_add_accepts_all_valid_senders() {
    let env = shared();

    for sender in &["operator", "system", "agent"] {
        let resp = env
            .router
            .dispatch(make_jsonrpc(
                &format!("s-{}", sender),
                "gate.add_message",
                serde_json::json!({
                    "gate_id": "sender-test",
                    "sender": sender,
                    "content": format!("msg from {}", sender),
                }),
            ))
            .await;
        assert!(resp.error.is_none(), "sender '{}' should be accepted: {:?}", sender, resp.error);
    }

    let get_resp = env
        .router
        .dispatch(make_jsonrpc(
            "get-senders",
            "gate.get_messages",
            serde_json::json!({ "gate_id": "sender-test" }),
        ))
        .await;
    let result = get_resp.result.expect("get result");
    let messages = result["messages"]
        .as_array()
        .expect("messages array");
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn gate_get_messages_empty_for_unknown_gate() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "empty",
            "gate.get_messages",
            serde_json::json!({ "gate_id": "nonexistent" }),
        ))
        .await;
    assert!(resp.error.is_none());
    let result = resp.result.expect("result");
    let messages = result["messages"]
        .as_array()
        .expect("messages array");
    assert!(messages.is_empty());
}

#[tokio::test]
async fn gate_routes_no_store_returns_error() {
    let env = shared();

    let resp = env
        .router_no_store
        .dispatch(make_jsonrpc(
            "ns1",
            "gate.get_messages",
            serde_json::json!({ "gate_id": "x" }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32000);
    assert!(err.message.contains("GatewayStore not available"));
}

#[tokio::test]
async fn gate_add_invalid_params_returns_parse_error() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "bad1",
            "gate.add_message",
            serde_json::json!({ "wrong_field": "x" }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("Invalid params"));
}

#[tokio::test]
async fn gate_get_invalid_params_returns_parse_error() {
    let env = shared();

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "bad2",
            "gate.get_messages",
            serde_json::json!({ "wrong_field": "x" }),
        ))
        .await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("should error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("Invalid params"));
}
