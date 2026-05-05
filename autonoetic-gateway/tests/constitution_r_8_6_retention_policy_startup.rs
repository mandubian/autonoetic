//! Constitution R-8.6 — retention policy is applied at gateway startup.

mod support;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::server::GatewayServer;
use autonoetic_types::causal_chain::CausalEventRecord;
use serial_test::serial;

struct EnvRestore {
    key: &'static str,
    previous: Option<String>,
}

impl EnvRestore {
    fn set(key: &'static str, value: impl Into<String>) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value.into());
        Self { key, previous }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

#[serial]
#[tokio::test(flavor = "current_thread")]
async fn r_8_6_retention_applies_during_server_bootstrap() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let mut config = workspace.gateway_config();
    config.retention.causal_events_days = 1;
    config.retention.execution_traces_days = 0;

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let stale_event_id = "evt-r-8-6-stale";
    store.create_causal_event(&CausalEventRecord {
        event_id: stale_event_id.to_string(),
        agent_id: "test-agent".to_string(),
        session_id: "session-r-8-6".to_string(),
        turn_id: Some("turn-0001".to_string()),
        event_seq: 1,
        timestamp: "2000-01-01T00:00:00Z".to_string(),
        category: "test".to_string(),
        action: "stale".to_string(),
        status: "SUCCESS".to_string(),
        enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: Some("stale fixture".to_string()),
    })?;

    let _shared_secret = EnvRestore::set("AUTONOETIC_SHARED_SECRET", "test-shared-secret");
    let _vault_key = EnvRestore::set(
        "AUTONOETIC_VAULT_KEY",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    let _vault_key_path = EnvRestore::set("AUTONOETIC_VAULT_KEY_PATH", "");

    // Force a fast startup failure after retention application by pre-binding OFP port.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
    let occupied_port = occupied.local_addr()?.port();
    config.ofp_port = occupied_port;
    config.port = 0;

    let server = GatewayServer::new(config.clone());
    let err = server
        .run()
        .await
        .expect_err("server should fail to bind due occupied OFP port");
    let err_text = err.to_string();
    assert!(
        err_text.contains("Address already in use")
            || err_text.contains("address already in use"),
        "expected bind failure after startup retention pass, got: {err_text}"
    );

    let store_after = GatewayStore::open(&gateway_dir)?;
    let events = store_after.search_causal_events(None, None, 200)?;
    assert!(
        !events.iter().any(|e| e.event_id == stale_event_id),
        "stale causal event should be pruned during startup retention pass"
    );
    assert!(
        events
            .iter()
            .any(|e| e.category == "retention" && e.action == "pruned"),
        "retention.pruned causal event should be emitted when pruning occurs"
    );

    Ok(())
}
