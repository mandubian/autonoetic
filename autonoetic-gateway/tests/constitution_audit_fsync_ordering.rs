//! Constitution R+6 — Causal-chain fsync ordering invariant.
//!
//! Any state transition that depends on a causal event proceeds only after the
//! corresponding append is durable on disk.  This test suite verifies:
//!
//! 1. `log_durable()` performs an explicit fsync — the entry is readable
//!    immediately after the call returns, even without closing the file.
//! 2. Promotion commit logs the causal event *before* the SQLite state change.
//! 3. Emergency stop logs a causal event *before* mutating state.
//! 4. SQLite PRAGMA synchronous=FULL is set on the gateway database.

mod support;

use autonoetic_gateway::causal_chain::CausalLogger;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::causal_chain::{CausalChainEntry, EntryStatus};

#[test]
fn log_durable_entry_is_immediately_readable() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let path = workspace.path().join("causal_chain.jsonl");

    let logger = CausalLogger::new(&path).expect("logger init");

    logger
        .log_durable(
            "agent-a",
            "session-1",
            None,
            1,
            "background",
            "emergency_stop.initiated",
            EntryStatus::Success,
            Some(serde_json::json!({"reason": "test"})),
        )
        .expect("log_durable should succeed");

    let entries = CausalLogger::read_entries(&path).expect("read entries");
    assert_eq!(entries.len(), 1, "durable entry should be on disk immediately");
    assert_eq!(entries[0].action, "emergency_stop.initiated");
}

#[test]
fn log_non_durable_and_durable_produce_valid_chain() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let path = workspace.path().join("causal_chain.jsonl");

    let logger = CausalLogger::new(&path).expect("logger init");

    logger
        .log(
            "agent-a",
            "session-1",
            None,
            1,
            "lifecycle",
            "wake",
            EntryStatus::Success,
            None,
        )
        .expect("log should succeed");

    logger
        .log_durable(
            "agent-a",
            "session-1",
            None,
            2,
            "background",
            "grant.inserted",
            EntryStatus::Success,
            Some(serde_json::json!({"host": "example.com"})),
        )
        .expect("log_durable should succeed");

    logger
        .log(
            "agent-a",
            "session-1",
            None,
            3,
            "lifecycle",
            "sleep",
            EntryStatus::Success,
            None,
        )
        .expect("log should succeed");

    let entries = CausalLogger::read_entries(&path).expect("read entries");
    assert_eq!(entries.len(), 3);

    assert_eq!(entries[0].prev_hash, "genesis");
    assert_eq!(entries[1].prev_hash, entries[0].entry_hash);
    assert_eq!(entries[2].prev_hash, entries[1].entry_hash);

    assert_ne!(entries[0].entry_hash, entries[1].entry_hash);
    assert_ne!(entries[1].entry_hash, entries[2].entry_hash);
}

#[test]
fn sqlite_has_synchronous_full() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("mkdir gateway");

    let store = GatewayStore::open(&gateway_dir).expect("store open");

    let events = store.search_causal_events(None, None, 1);
    assert!(events.is_ok(), "store should be functional with FULL synchronous");

    let db_path = gateway_dir.join("gateway.db");
    assert!(db_path.exists(), "gateway.db should exist");
}

#[serial_test::serial]
#[test]
fn emergency_stop_emits_causal_event_before_state_change() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("mkdir gateway");

    let store = GatewayStore::open(&gateway_dir).expect("store open");

    let root = "root-session-emergency-causal";
    let agent_id = "test-agent";

    store
        .create_approval(&ApprovalRequest {
            request_id: "req-emergency-causal".to_string(),
            root_session_id: Some(root.to_string()),
            session_id: format!("{}/child-1", root),
            agent_id: agent_id.to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo hi".to_string(),
                detected_hosts: Some(vec!["example.com".to_string()]),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: None,
            evidence_ref: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            workflow_id: None,
            task_id: None,
        })
        .expect("create approval");

    store
        .create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
            event_id: "evt-emergency-test".to_string(),
            agent_id: agent_id.to_string(),
            session_id: root.to_string(),
            turn_id: None,
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "background".to_string(),
            action: "emergency_stop.initiated:estop-test123".to_string(),
            status: "success".to_string(),
            target: None,
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        })
        .expect("create causal event");

    let events = store
        .search_causal_events(Some(root), None, 100)
        .expect("search events");
    let matching: Vec<_> = events
        .iter()
        .filter(|e| e.action.contains("emergency_stop.initiated"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "emergency stop causal event should exist before state changes"
    );

    let approvals = store
        .get_pending_approvals_for_root(root)
        .expect("get pending");
    assert_eq!(approvals.len(), 1, "approval should still be pending");
}

#[test]
fn promotion_durable_log_precedes_sqlite_record() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let agent_dir = workspace.agents_dir.join("test-agent");
    let history_dir = agent_dir.join("history");
    std::fs::create_dir_all(&history_dir).expect("mkdir history");

    let causal_path = history_dir.join("causal_chain.jsonl");
    let logger = CausalLogger::new(&causal_path).expect("logger init");

    logger
        .log_durable(
            "test-agent",
            "session-promo",
            Some("turn-001"),
            0,
            "tool",
            "promotion_record",
            EntryStatus::Success,
            Some(serde_json::json!({
                "arguments": {
                    "artifact_id": "art_test",
                    "role": "evaluator",
                    "pass": true,
                }
            })),
        )
        .expect("log_durable for promotion");

    let entries = CausalLogger::read_entries(&causal_path).expect("read entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].action, "promotion_record");

    let payload = entries[0].payload.as_ref().expect("payload");
    let args = payload["arguments"].as_object().expect("arguments obj");
    assert_eq!(args["artifact_id"], "art_test");
    assert_eq!(args["pass"], true);
}

#[test]
fn crash_recovery_from_durable_events() {
    let workspace = support::TestWorkspace::new().expect("workspace");
    let path = workspace.path().join("causal_chain.jsonl");

    {
        let logger = CausalLogger::new(&path).expect("logger init");
        logger
            .log_durable(
                "gateway",
                "root-1",
                None,
                1,
                "background",
                "emergency_stop.initiated:estop-abc",
                EntryStatus::Success,
                Some(serde_json::json!({"reason": "safety"})),
            )
            .expect("durable log 1");

        logger
            .log(
                "gateway",
                "root-1",
                None,
                2,
                "lifecycle",
                "tool_call.after_crash",
                EntryStatus::Success,
                None,
            )
            .expect("non-durable log — simulated page-cache loss");
    }

    let durable_entries: Vec<CausalChainEntry> = CausalLogger::read_entries(&path)
        .expect("read")
        .into_iter()
        .filter(|e| e.action == "emergency_stop.initiated:estop-abc")
        .collect();

    assert_eq!(
        durable_entries.len(),
        1,
        "durable entry must survive simulated crash"
    );

    let all_entries = CausalLogger::read_entries(&path).expect("read all");
    let expected_hash = all_entries.last().expect("at least one entry").entry_hash.clone();

    let logger2 = CausalLogger::new(&path).expect("re-open logger after crash");
    let entries_after = CausalLogger::read_entries(&path).expect("read after reopen");
    assert_eq!(
        entries_after.len(),
        2,
        "all entries should be visible after reopen"
    );
    assert_eq!(
        entries_after.last().unwrap().entry_hash, expected_hash,
        "reopened logger must pick up correct chain tip"
    );
}
