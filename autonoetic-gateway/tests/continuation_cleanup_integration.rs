//! Integration tests for issue #87: continuation cleanup on reject/withdraw
//! and startup reaper for orphaned continuation files.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::runtime::continuation;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use serial_test::serial;
use std::path::PathBuf;

fn make_gateway_dir(tmp: &tempfile::TempDir) -> PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    gw
}

fn make_config(tmp: &tempfile::TempDir) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.agents_dir = tmp.path().to_path_buf();
    config
}

fn seed_approval(store: &GatewayStore, task_id: &str) -> String {
    let request_id = format!("req-{}", task_id);
    let request = ApprovalRequest {
        request_id: request_id.clone(),
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        root_session_id: Some("root-1".to_string()),
        action: ScheduledAction::SandboxExec {
            command: "echo hello".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.example.com".to_string()]),
        },
        reason: Some("test".to_string()),
        evidence_ref: None,
        status: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        decided_at: None,
        decided_by: None,
        workflow_id: Some("wf-1".to_string()),
        task_id: Some(task_id.to_string()),
        approval_level: ApprovalLevel::Operator,
        decision_reason: None,
        similar_to_request_id: None,
        similarity_score: None,
    };
    store.create_approval(&request).unwrap();
    request_id
}

fn write_dummy_continuation(config: &GatewayConfig, task_id: &str) {
    let dir = continuation::continuations_dir(config);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.json", task_id));
    std::fs::write(&path, r#"{"test": true}"#).unwrap();
}

fn continuation_exists(config: &GatewayConfig, task_id: &str) -> bool {
    continuation::continuation_path(config, task_id).exists()
}

#[test]
#[serial]
fn test_reject_deletes_continuation() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let task_id = "task-reject-1";
    let request_id = seed_approval(&store, task_id);
    write_dummy_continuation(&config, task_id);
    assert!(continuation_exists(&config, task_id));

    let _ = autonoetic_gateway::scheduler::reject_request(
        &config,
        Some(&store),
        &request_id,
        "cli",
        Some("testing".to_string()),
        None,
    )
    .unwrap();

    assert!(
        !continuation_exists(&config, task_id),
        "Continuation file should be deleted after rejection"
    );
}

#[test]
#[serial]
fn test_cancel_deletes_continuation() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let task_id = "task-cancel-1";
    let request_id = seed_approval(&store, task_id);
    write_dummy_continuation(&config, task_id);

    let _ = autonoetic_gateway::scheduler::cancel_request(
        &config,
        Some(&store),
        &request_id,
        "cli",
        Some("testing".to_string()),
        None,
    )
    .unwrap();

    assert!(
        !continuation_exists(&config, task_id),
        "Continuation file should be deleted after cancellation"
    );
}

#[test]
#[serial]
fn test_reaper_removes_terminal_approval_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let task_id = "task-orphan-terminal";
    let request_id = seed_approval(&store, task_id);
    write_dummy_continuation(&config, task_id);
    assert!(continuation_exists(&config, task_id));

    store
        .record_decision(&request_id, "rejected", "test", &chrono::Utc::now().to_rfc3339(), None)
        .unwrap();

    let reaped = continuation::reap_orphaned_continuations(&config, &store).unwrap();
    assert_eq!(reaped, 1);
    assert!(
        !continuation_exists(&config, task_id),
        "Reaper should remove continuation with terminal approval"
    );
}

#[test]
#[serial]
fn test_reaper_removes_missing_approval_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let task_id = "task-orphan-missing";
    write_dummy_continuation(&config, task_id);
    assert!(continuation_exists(&config, task_id));

    let reaped = continuation::reap_orphaned_continuations(&config, &store).unwrap();
    assert_eq!(reaped, 1);
    assert!(
        !continuation_exists(&config, task_id),
        "Reaper should remove continuation with no approval row"
    );
}

#[test]
#[serial]
fn test_reaper_preserves_pending_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let task_id = "task-pending-keep";
    let _request_id = seed_approval(&store, task_id);
    write_dummy_continuation(&config, task_id);

    let reaped = continuation::reap_orphaned_continuations(&config, &store).unwrap();
    assert_eq!(reaped, 0);
    assert!(
        continuation_exists(&config, task_id),
        "Reaper should NOT remove continuation with pending approval"
    );
}

#[test]
#[serial]
fn test_reaper_handles_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_config(&tmp);
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();

    let reaped = continuation::reap_orphaned_continuations(&config, &store).unwrap();
    assert_eq!(reaped, 0);
}
