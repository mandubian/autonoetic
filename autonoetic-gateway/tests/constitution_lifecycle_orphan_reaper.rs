//! Constitution R+12 — Orphan-child reaper on parent termination.

mod support;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::reap_orphaned_sessions;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::causal_chain::SessionTranscriptRecord;
use autonoetic_types::config::GatewayConfig;
use support::TestWorkspace;

fn make_transcript(
    session_id: &str,
    root_session_id: &str,
    agent_id: &str,
    status: &str,
) -> SessionTranscriptRecord {
    let now = chrono::Utc::now().to_rfc3339();
    SessionTranscriptRecord {
        transcript_id: format!("tid-{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
        session_id: session_id.to_string(),
        root_session_id: root_session_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: None,
        user_id: None,
        started_at: now.clone(),
        ended_at: if status != "active" { Some(now) } else { None },
        status: status.to_string(),
        turn_count: 0,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    }
}

#[tokio::test]
async fn orphan_reaper_cancels_child_when_parent_ends() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-001";
    let parent_id = "root-session-001/planner.default-abcd1234";
    let child_id = "root-session-001/planner.default-abcd1234/coder.default-efgh5678";

    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "completed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "planner.default",
            "failed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child transcript should exist");
    assert_eq!(
        child.status, "failed",
        "orphaned child should be marked as failed"
    );

    let events = store
        .search_causal_events(Some(child_id), None, 100)
        .unwrap();
    let reaped = events.iter().find(|e| e.action == "parent_terminated");
    assert!(
        reaped.is_some(),
        "should emit parent_terminated causal event"
    );
    let ev = reaped.unwrap();
    assert_eq!(ev.category, "session");
    assert_eq!(ev.status, "error");
    assert!(ev.enforced_rules.contains(&"R+12".to_string()));
    assert_eq!(ev.target.as_deref(), Some(parent_id));
}

#[tokio::test]
async fn orphan_reaper_ignores_active_parent() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-002";
    let parent_id = "root-session-002/planner.default-aaaa1111";
    let child_id = "root-session-002/planner.default-aaaa1111/coder.default-bbbb2222";

    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "planner.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child should exist");
    assert_eq!(
        child.status, "active",
        "child with active parent should NOT be reaped"
    );
}

#[tokio::test]
async fn orphan_reaper_noop_without_store() {
    let ws = TestWorkspace::new().unwrap();
    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, None));
    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed without store");
}

#[tokio::test]
async fn orphan_reaper_handles_multiple_orphans() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-003";
    let parent_id = "root-session-003/planner.default-cccc3333";
    let child1_id = "root-session-003/planner.default-cccc3333/coder.default-dddd4444";
    let child2_id = "root-session-003/planner.default-cccc3333/researcher.default-eeee5555";

    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "completed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "planner.default",
            "completed",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child1_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child2_id,
            root_id,
            "researcher.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    for cid in &[child1_id, child2_id] {
        let child = store
            .find_transcript_by_session_id(cid)
            .unwrap()
            .expect("child should exist");
        assert_eq!(
            child.status, "failed",
            "orphaned child {} should be marked as failed",
            cid
        );
    }
}

#[tokio::test]
async fn orphan_reaper_does_not_reap_suspended_parent() {
    let ws = TestWorkspace::new().unwrap();
    let gateway_dir = ws.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_id = "root-session-004";
    let parent_id = "root-session-004/planner.default-ffff6666";
    let child_id = "root-session-004/planner.default-ffff6666/coder.default-gggg7777";

    store
        .upsert_session_transcript(&make_transcript(
            root_id,
            root_id,
            "planner.default",
            "active",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            parent_id,
            root_id,
            "planner.default",
            "suspended",
        ))
        .unwrap();
    store
        .upsert_session_transcript(&make_transcript(
            child_id,
            root_id,
            "coder.default",
            "active",
        ))
        .unwrap();

    let config = ws.gateway_config();
    let execution = std::sync::Arc::new(GatewayExecutionService::new(config, Some(store.clone())));

    reap_orphaned_sessions(execution)
        .await
        .expect("reaper should succeed");

    let child = store
        .find_transcript_by_session_id(child_id)
        .unwrap()
        .expect("child should exist");
    assert_eq!(
        child.status, "active",
        "child with suspended parent should NOT be reaped"
    );
}
