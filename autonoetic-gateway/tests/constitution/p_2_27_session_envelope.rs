//! Constitutional test for P-2.27: session capability envelope.
//!
//! Verifies:
//! 1. Envelope grants dedup against tool calls (locked envelope → grants cover targets).
//! 2. Envelope hosts are derived from execution_traces (mechanical, not LLM).
//! 3. Emergency stop revokes all session envelopes for the root session.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::runtime::session_envelope::propose_discovered_envelope;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use tempfile::tempdir;

fn curl_trace(session_id: &str, command: &str) -> ExecutionTraceRecord {
    ExecutionTraceRecord {
        trace_id: format!("trace-{}", uuid::Uuid::new_v4()),
        event_id: None,
        agent_id: "researcher.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: "2026-06-15T12:00:00Z".to_string(),
        tool_name: "sandbox_exec".to_string(),
        command: Some(command.to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 10,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: Some(format!(r#"{{"command":"{command}"}}"#)),
        result: None,
        egress_label: None,
    }
}

#[test]
fn envelope_grants_cover_observed_hosts() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let root = "root-p227-grants";

    store
        .create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))
        .unwrap();

    let proposal =
        propose_discovered_envelope(&store, root, "discovered", None, "operator")
            .unwrap()
            .expect("proposal");
    assert!(!proposal.skipped);

    assert!(
        store.session_grants_cover_targets(root, "researcher.default", &["api.open-meteo.com".to_string()]),
        "auto-locked discovered envelope must cover observed host"
    );
    assert_eq!(store.get_active_envelopes(root).unwrap().len(), 1);
}

#[test]
fn envelope_hosts_derived_from_execution_traces_not_llm() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let root = "root-p227-mechanical";

    store
        .create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))
        .unwrap();
    store
        .create_execution_trace(&curl_trace(
            root,
            "curl -s https://geocoding-api.open-meteo.com/v1/search?name=Paris",
        ))
        .unwrap();

    let hosts = store.discover_observed_hosts(root).unwrap();
    assert_eq!(
        hosts,
        vec![
            "api.open-meteo.com".to_string(),
            "geocoding-api.open-meteo.com".to_string(),
        ],
        "discovered hosts must match execution traces exactly"
    );
}

#[test]
fn emergency_stop_revokes_session_envelopes() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let root = "root-p227-estop";

    store
        .create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))
        .unwrap();

    let proposal =
        propose_discovered_envelope(&store, root, "discovered", None, "operator")
            .unwrap()
            .expect("proposal");
    let _ = proposal;

    assert_eq!(store.get_active_envelopes(root).unwrap().len(), 1);

    let revoked = store
        .revoke_session_envelopes_for_root(root)
        .unwrap();
    assert_eq!(revoked, 1, "emergency stop must revoke the envelope");

    assert!(
        store.get_active_envelopes(root).unwrap().is_empty(),
        "no active envelopes after emergency stop"
    );
    assert!(
        store.get_proposed_envelopes(root).unwrap().is_empty(),
        "no proposed envelopes after emergency stop"
    );
}
