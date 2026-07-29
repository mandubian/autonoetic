//! `content.project_live` JSON-RPC — materialize live session drafts into a real
//! directory the operator can open in an external editor (#524, Tier 1).


mod support;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use crate::support::TestWorkspace;

struct Env {
    _ws: TestWorkspace,
    router: JsonRpcRouter,
    gateway_dir: PathBuf,
}

static SHARED: OnceLock<Env> = OnceLock::new();

// `JsonRpcRouter::new` initializes the process-global constitution runtime once,
// so all tests share one router.
fn shared() -> &'static Env {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let config = ws.gateway_config();
        let gateway_dir = ws.path().join("agents").join(".gateway");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(config, Some(store));
        Env {
            _ws: ws,
            router,
            gateway_dir,
        }
    })
}

fn req(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "proj-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

#[tokio::test]
async fn content_project_live_writes_real_files() {
    let env = shared();
    let session = "root-projlive";

    let cs = ContentStore::new(&env.gateway_dir).unwrap();
    let h = cs.write(b"port: 8080\n").unwrap();
    cs.register_name(session, "config.yaml", &h).unwrap();

    let resp = env
        .router
        .dispatch(req(
            "content.project_live",
            serde_json::json!({ "session_id": session }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let r = resp.result.expect("result");
    assert_eq!(r["ok"], true);
    assert_eq!(r["count"], 1);
    let path = r["path"].as_str().expect("path");
    // The projected file exists on disk with the real bytes.
    let on_disk = std::fs::read(std::path::Path::new(path).join("config.yaml")).unwrap();
    assert_eq!(on_disk, b"port: 8080\n");
}

#[tokio::test]
async fn content_project_live_empty_session_is_ok() {
    let env = shared();
    let resp = env
        .router
        .dispatch(req(
            "content.project_live",
            serde_json::json!({ "session_id": "root-projlive-empty" }),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    assert_eq!(resp.result.expect("result")["count"], 0);
}
