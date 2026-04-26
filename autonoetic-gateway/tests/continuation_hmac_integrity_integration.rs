//! Integration tests for continuation file HMAC integrity and action-equality.
//!
//! Verifies:
//!   1. HMAC-signed continuations load and verify correctly.
//!   2. Tampered continuation files are rejected with an error.
//!   3. Action mismatch between continuation and approval is detected.

mod support;

use autonoetic_gateway::runtime::continuation::{
    continuation_hmac_key, continuations_dir, load_continuation, save_continuation,
    PendingApprovalToolCall, SignedContinuation, TurnContinuation,
};
use autonoetic_gateway::runtime::guard::LoopGuardState;
use autonoetic_gateway::llm::{Message, Role, ToolCall};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::config::GatewayConfig;

fn default_guard_state() -> LoopGuardState {
    LoopGuardState {
        max_loops_without_progress: 10,
        max_tool_failures: 5,
        max_consecutive_same_progress: 2,
        max_child_failures: 3,
        current_loops: 0,
        tool_failure_counts: std::collections::HashMap::new(),
        last_progress_fingerprint: None,
        consecutive_progress_count: 0,
        child_failure_count: 0,
    }
}

fn make_test_continuation(request_id: &str) -> TurnContinuation {
    TurnContinuation {
        history: vec![Message {
            role: Role::User,
            content: "test prompt".to_string(),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        assistant_message: Message {
            role: Role::Assistant,
            content: "".to_string(),
            tool_calls: vec![ToolCall {
                id: "call_test".to_string(),
                name: "sandbox_exec".to_string(),
                arguments: "{}".to_string(),
            }],
            tool_call_id: None,
        },
        completed_tool_results: vec![],
        pending_tool_call: PendingApprovalToolCall {
            call_id: "call_test".to_string(),
            tool_name: "sandbox_exec".to_string(),
            arguments: "{}".to_string(),
            approval_response: "{}".to_string(),
        },
        remaining_tool_calls: vec![],
        approval_request_id: request_id.to_string(),
        pending_action: Some(ScheduledAction::SandboxExec {
            command: "echo hello".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        }),
        workflow_id: None,
        task_id: Some("task-001".to_string()),
        session_id: "sess-001".to_string(),
        turn_id: "turn-001".to_string(),
        suspended_at: chrono::Utc::now().to_rfc3339(),
        loop_guard_state: default_guard_state(),
    }
}

fn make_test_config(dir: &std::path::Path) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = dir.to_path_buf();
    config
}

#[test]
#[serial_test::serial]
fn test_hmac_signed_continuation_roundtrips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = make_test_config(tmp.path());
    let cont = make_test_continuation("req-001");

    save_continuation(&config, "task-001", &cont).expect("save");

    let loaded = load_continuation(&config, "task-001")
        .expect("load")
        .expect("some");

    assert_eq!(loaded.approval_request_id, "req-001");
    assert_eq!(loaded.session_id, "sess-001");
    assert!(loaded.pending_action.is_some());
}

#[test]
#[serial_test::serial]
fn test_tampered_payload_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = make_test_config(tmp.path());
    let cont = make_test_continuation("req-001");

    save_continuation(&config, "task-001", &cont).expect("save");

    let dir = continuations_dir(&config);
    let path = dir.join("task-001.json");
    let raw = std::fs::read_to_string(&path).expect("read");

    let mut envelope: SignedContinuation = serde_json::from_str(&raw).expect("parse envelope");
    envelope.payload_json = envelope.payload_json.replace("echo hello", "rm -rf /");
    std::fs::write(&path, serde_json::to_string_pretty(&envelope).expect("serialize"))
        .expect("write tampered");

    let result = load_continuation(&config, "task-001");
    assert!(result.is_err(), "tampered continuation should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("integrity violation"),
        "error should mention integrity violation, got: {}",
        err
    );
}

#[test]
#[serial_test::serial]
fn test_tampered_hmac_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = make_test_config(tmp.path());
    let cont = make_test_continuation("req-002");

    save_continuation(&config, "task-002", &cont).expect("save");

    let dir = continuations_dir(&config);
    let path = dir.join("task-002.json");
    let raw = std::fs::read_to_string(&path).expect("read");

    let mut envelope: SignedContinuation = serde_json::from_str(&raw).expect("parse envelope");
    envelope.hmac_hex = "0000000000000000000000000000000000000000000000000000000000000000".to_string();
    std::fs::write(&path, serde_json::to_string_pretty(&envelope).expect("serialize"))
        .expect("write tampered hmac");

    let result = load_continuation(&config, "task-002");
    assert!(result.is_err(), "tampered HMAC should be rejected");
}

#[test]
#[serial_test::serial]
fn test_unsigned_legacy_continuation_still_loads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = make_test_config(tmp.path());

    let cont = make_test_continuation("req-legacy");
    let raw_payload = serde_json::to_string_pretty(&cont).expect("serialize");

    let dir = continuations_dir(&config);
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("task-legacy.json");
    std::fs::write(&path, &raw_payload).expect("write unsigned");

    let loaded = load_continuation(&config, "task-legacy")
        .expect("load legacy")
        .expect("some");

    assert_eq!(loaded.approval_request_id, "req-legacy");
}

#[test]
#[serial_test::serial]
fn test_continuation_key_derivation_uses_node_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config1 = make_test_config(tmp.path());
    config1.node_id = "node-alpha".to_string();

    let mut config2 = make_test_config(tmp.path());
    config2.node_id = "node-beta".to_string();

    let key1 = continuation_hmac_key(&config1);
    let key2 = continuation_hmac_key(&config2);

    assert_ne!(key1, key2, "different node_id should produce different keys");
}

#[test]
#[serial_test::serial]
fn test_continuation_key_explicit_overrides_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut config = make_test_config(tmp.path());
    config.node_id = "default-node".to_string();
    config.continuation_key = Some("my-explicit-key".to_string());

    let key = continuation_hmac_key(&config);
    assert_eq!(key, "my-explicit-key");
}

#[test]
#[serial_test::serial]
fn test_missing_continuation_returns_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = make_test_config(tmp.path());

    let result = load_continuation(&config, "nonexistent").expect("load");
    assert!(result.is_none());
}

#[test]
#[serial_test::serial]
fn test_different_key_rejects_continuation() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let mut config_save = make_test_config(tmp.path());
    config_save.continuation_key = Some("save-key".to_string());

    let mut config_load = make_test_config(tmp.path());
    config_load.continuation_key = Some("different-key".to_string());

    let cont = make_test_continuation("req-key-001");
    save_continuation(&config_save, "task-key-001", &cont).expect("save");

    let result = load_continuation(&config_load, "task-key-001");
    assert!(result.is_err(), "different key should reject");
}
