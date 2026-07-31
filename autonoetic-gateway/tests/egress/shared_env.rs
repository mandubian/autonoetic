//! One router/store for the whole egress test binary (#922, #978).
//!
//! `JsonRpcRouter::new` initializes the process-global constitution runtime,
//! which cannot switch constitutional configs in one process — so every
//! router-driving suite in this grouped binary shares a single env, exactly
//! like `tests/content/comment.rs` ("all tests share a single router/env").
//! Suites isolate via unique session ids, event ids, and content names.

use autonoetic_gateway::router::JsonRpcRouter;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use std::sync::{Arc, OnceLock};

pub struct SharedEnv {
    _tmp: tempfile::TempDir,
    pub store: Arc<GatewayStore>,
    pub router: JsonRpcRouter,
}

pub static ENV: OnceLock<SharedEnv> = OnceLock::new();

pub fn env() -> &'static SharedEnv {
    ENV.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("tempdir");
        // JsonRpcRouter::new reads the constitution from `agents_dir`, so point
        // it at the temp workspace (an absent constitution dir is tolerated by
        // the init path used in tests).
        let mut config = GatewayConfig::default();
        config.agents_dir = tmp.path().to_path_buf();
        let store = Arc::new(GatewayStore::open(tmp.path()).expect("store open"));
        let router = JsonRpcRouter::new(config, Some(store.clone()));
        SharedEnv {
            _tmp: tmp,
            store,
            router,
        }
    })
}
