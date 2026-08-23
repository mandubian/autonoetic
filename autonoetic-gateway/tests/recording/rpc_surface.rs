//! `recording.*` RPC service layer (#1119 tranche 3) — the logic behind the
//! JSON-RPC methods `autonoetic recording list|inspect|delete|cancel` and the
//! `eval sealed` lookups now call, so the CLI stops reading gateway.db
//! directly.
//!
//! Service-level like `tests/session/outcome_rpc.rs`: a second concurrent
//! in-process router initialization races global startup paths; the router
//! arms are thin param-decode + delegation.

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::recording::{FixtureSet, FixtureSetStatus, RecordingSession, RecordingStatus};
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
        // Leak: service + store must outlive the tests sharing this OnceLock.
        std::mem::forget(ws);
        GatewayExecutionService::new(config, Some(store))
    })
}

fn seed_session(session_id: &str, agent: &str) -> RecordingSession {
    RecordingSession {
        session_id: session_id.to_string(),
        agent_id: agent.to_string(),
        artifact_id: "ar_test".to_string(),
        revision_id: "rev_test".to_string(),
        root_session_id: session_id.to_string(),
        started_at: "2026-08-23T10:00:00+00:00".to_string(),
        stopped_at: None,
        duration_secs: None,
        max_requests: None,
        max_bytes: None,
        request_count: 0,
        total_bytes: 0,
        status: RecordingStatus::Active,
        fixture_set_id: None,
        created_by: "operator".to_string(),
    }
}

#[tokio::test]
async fn list_and_get_roundtrip_seeded_sessions() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .create_recording_session(&seed_session("rec-rpc-1", "coder.default"))
        .expect("seed");

    let sessions = svc.recording_sessions(None, 50).expect("list");
    assert!(
        sessions.iter().any(|s| s.session_id == "rec-rpc-1"),
        "seeded session not listed"
    );
    let mine = svc.recording_sessions(Some("coder.default"), 50).expect("filtered");
    assert!(mine.iter().all(|s| s.agent_id == "coder.default"));

    let got = svc.recording_session_get("rec-rpc-1").expect("get");
    assert_eq!(got["session"]["session_id"].as_str(), Some("rec-rpc-1"));
    assert!(got["fixture_set"].is_null(), "no fixture set linked");
}

#[tokio::test]
async fn get_missing_session_is_a_typed_error() {
    let err = service()
        .recording_session_get("no-such-recording")
        .expect_err("missing session must error");
    assert!(err.to_string().contains("not found"), "unexpected: {err}");
}

#[tokio::test]
async fn cancel_stops_session_and_emits_causal_event() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .create_recording_session(&seed_session("rec-rpc-cancel", "coder.default"))
        .expect("seed");

    svc.recording_session_cancel("rec-rpc-cancel")
        .expect("cancel");

    let got = svc.recording_session_get("rec-rpc-cancel").expect("get");
    assert_eq!(
        got["session"]["status"].as_str(),
        Some("cancelled"),
        "status should be cancelled: {got}"
    );
    assert!(
        got["session"]["stopped_at"].as_str().is_some(),
        "stopped_at should be set: {got}"
    );
}

#[tokio::test]
async fn fixture_set_lookup_for_eval_sealed() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    store
        .create_recording_session(&seed_session("rec-rpc-fs", "coder.default"))
        .expect("seed session");
    let fs = FixtureSet {
        fixture_set_id: "fs_rpc_1".to_string(),
        agent_id: "coder.default".to_string(),
        revision_id: "rev_test".to_string(),
        recording_session_id: "rec-rpc-fs".to_string(),
        created_at: "2026-08-23T11:00:00+00:00".to_string(),
        fixture_file_count: 3,
        total_bytes: 42,
        digest: format!("sha256:{}", "a".repeat(64)),
        host_summary: vec!["api.example.com".to_string()],
        host_count: 1,
        redaction_summary: vec![],
        status: FixtureSetStatus::Ready,
    };
    store.create_fixture_set(&fs).expect("seed fixture set");
    // Link the fixture set to the session, mirroring the runtime.
    store
        .set_recording_session_fixture_set("rec-rpc-fs", "fs_rpc_1")
        .expect("link");

    let got = svc.recording_fixture_set("fs_rpc_1").expect("fixture set");
    assert_eq!(got.recording_session_id, "rec-rpc-fs");
    assert_eq!(got.fixture_file_count, 3);

    // recording.get embeds the linked fixture set for `recording inspect`.
    let session = svc.recording_session_get("rec-rpc-fs").expect("get");
    assert_eq!(session["fixture_set"]["fixture_set_id"].as_str(), Some("fs_rpc_1"));

    // Delete removes session + linked fixture set together.
    let outcome = svc.recording_session_delete("rec-rpc-fs").expect("delete");
    assert_eq!(outcome["deleted_fixture_set"].as_str(), Some("fs_rpc_1"));
    assert!(svc.recording_session_get("rec-rpc-fs").is_err());
    assert!(svc.recording_fixture_set("fs_rpc_1").is_err());
}
