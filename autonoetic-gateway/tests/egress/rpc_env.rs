//! One JSON-RPC router + store shared by every RPC suite in this binary.
//!
//! `JsonRpcRouter::new` initializes the **process-global** constitution runtime,
//! and that runtime refuses to switch `gateway_dir` mid-process (the drift guard
//! in `constitution_digest::ensure_same_runtime_config`). So two suites each
//! standing up their own router in their own tempdir makes whichever runs second
//! panic with `constitution runtime already initialized`, and which one that is
//! depends on test scheduling — a failure that looks like flakiness and has
//! nothing to do with either suite's subject.
//!
//! One env for the whole binary removes the hazard instead of ordering around
//! it. Suites stay isolated the way the rest of this binary is: by using
//! distinct session ids, not distinct workspaces.

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use std::sync::{Arc, OnceLock};

pub struct Env {
    _tmp: tempfile::TempDir,
    pub store: Arc<GatewayStore>,
    pub router: JsonRpcRouter,
}

static ENV: OnceLock<Env> = OnceLock::new();

pub fn env() -> &'static Env {
    ENV.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("tempdir");
        // `JsonRpcRouter::new` reads the constitution relative to `agents_dir`,
        // so point it at the temp workspace (an absent constitution dir is
        // tolerated by the init path used in tests).
        let mut config = GatewayConfig::default();
        config.agents_dir = tmp.path().to_path_buf();
        config.runtime_dir = config.agents_dir.join(".gateway");
        let store = Arc::new(GatewayStore::open(tmp.path()).expect("store open"));
        let router = JsonRpcRouter::new(config, Some(store.clone()));
        Env {
            _tmp: tmp,
            store,
            router,
        }
    })
}

/// Dispatch one request against the shared router. `id` is the caller's label,
/// so a failure names the suite that made the call.
pub async fn rpc_as(id: &str, method: &str, params: serde_json::Value) -> JsonRpcResponse {
    env()
        .router
        .dispatch(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params,
            auth_token: None,
        })
        .await
}
