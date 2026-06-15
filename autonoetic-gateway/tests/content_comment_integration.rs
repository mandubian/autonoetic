//! `content.comment` JSON-RPC — operator comments anchored to a live content
//! file, recorded on the timeline and delivered to the owning agent at its next
//! turn (#521). See `docs/design/operator-live-comments.md`.

mod support;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::SessionAgentBinding;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use support::TestWorkspace;

struct Env {
    _ws: TestWorkspace,
    store: Arc<GatewayStore>,
    router: JsonRpcRouter,
    gateway_dir: PathBuf,
}

static SHARED: OnceLock<Env> = OnceLock::new();

// `JsonRpcRouter::new` initializes the process-global constitution runtime once
// per process, so all tests share a single router/env. Tests isolate via unique
// session ids and content names.
fn shared() -> &'static Env {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let config = ws.gateway_config();
        // gateway_root_dir(config) == agents_dir/.gateway (where the router's
        // ContentStore lives); the passed-in GatewayStore is at ws.path().
        let gateway_dir = ws.path().join("agents").join(".gateway");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(config, Some(store.clone()));
        Env {
            _ws: ws,
            store,
            router,
            gateway_dir,
        }
    })
}

fn req(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "cmt-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

// Seed a session→agent binding so event.ingest target resolution succeeds.
// (The background turn it spawns fails harmlessly — no real revision/LLM — but
// content.comment returns as soon as the async ingest is accepted.)
fn seed_binding(store: &GatewayStore, session_id: &str) {
    store
        .upsert_session_agent_binding(&SessionAgentBinding {
            session_id: session_id.to_string(),
            root_session_id: session_id.to_string(),
            alias_id: Some("planner.default".to_string()),
            agent_id: "planner.default".to_string(),
            revision_id: "rev_seed".to_string(),
            runtime_lock_hash: "sha256:seed".to_string(),
            home_node_id: "gateway".to_string(),
            created_at: "2026-06-15T00:00:00Z".to_string(),
            requested_target: "planner.default".to_string(),
        })
        .unwrap();
}

async fn timeline_event_types(env: &Env, root: &str) -> Vec<String> {
    let resp = env
        .router
        .dispatch(req(
            "session.timeline.list",
            serde_json::json!({ "root_session_id": root, "min_altitude": "detail", "limit": 200 }),
        ))
        .await;
    resp.result
        .and_then(|r| r["entries"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| e["event_type"].as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn content_comment_happy_path_acks_and_emits_timeline() {
    let env = shared();
    let root = "root-cmt-happy";
    seed_binding(&env.store, root);

    let cs = ContentStore::new(&env.gateway_dir).unwrap();
    let handle = cs.write(b"port: 8080\nsecret: hunter2\n").unwrap();
    cs.register_name(root, "config.yaml", &handle).unwrap();

    let resp = env
        .router
        .dispatch(req(
            "content.comment",
            serde_json::json!({
                "session_id": root,
                "name": "config.yaml",
                "handle": handle,
                "line_start": 2,
                "body": "you hardcoded a secret here",
            }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let r = resp.result.expect("result");
    assert_eq!(r["ok"], true);
    assert_eq!(r["drifted"], false);
    assert!(
        r["comment_id"].as_str().unwrap_or("").starts_with("cmt_"),
        "comment_id: {:?}",
        r["comment_id"]
    );

    let types = timeline_event_types(env, root).await;
    assert!(
        types.iter().any(|t| t == "operator.comment"),
        "expected an operator.comment timeline event, got: {types:?}"
    );
}

#[tokio::test]
async fn content_comment_flags_drift_when_file_changed() {
    let env = shared();
    let root = "root-cmt-drift";
    seed_binding(&env.store, root);

    let cs = ContentStore::new(&env.gateway_dir).unwrap();
    let old = cs.write(b"v1\n").unwrap();
    let new = cs.write(b"v2\n").unwrap();
    // Name now points at the new version; the operator comments on the old one.
    cs.register_name(root, "drift.txt", &new).unwrap();

    let resp = env
        .router
        .dispatch(req(
            "content.comment",
            serde_json::json!({
                "session_id": root,
                "name": "drift.txt",
                "handle": old,
                "body": "this line looks wrong",
            }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    assert_eq!(resp.result.expect("result")["drifted"], true);
}

#[tokio::test]
async fn content_comment_rejects_empty_body() {
    let env = shared();
    let resp = env
        .router
        .dispatch(req(
            "content.comment",
            serde_json::json!({ "session_id": "root-cmt-empty", "name": "x", "body": "   " }),
        ))
        .await;
    let err = resp.error.expect("error");
    assert_eq!(err.code, -32602, "{}", err.message);
}

#[tokio::test]
async fn content_comment_unknown_name_is_error() {
    let env = shared();
    let resp = env
        .router
        .dispatch(req(
            "content.comment",
            serde_json::json!({
                "session_id": "root-cmt-unknown",
                "name": "does-not-exist.txt",
                "body": "hello",
            }),
        ))
        .await;
    let err = resp.error.expect("error");
    assert_eq!(err.code, -32000, "{}", err.message);
}
