//! `session.timeline.list` JSON-RPC (#391) — the canonical Session Room timeline
//! served over the gateway API so channels are clients, not direct store readers.

mod support;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::{GatewayStore, LiveDigestEventRecord};
use std::sync::{Arc, OnceLock};
use support::TestWorkspace;

struct SharedEnv {
    _ws: TestWorkspace,
    store: Arc<GatewayStore>,
    router: JsonRpcRouter,
}

static SHARED: OnceLock<SharedEnv> = OnceLock::new();

// One shared workspace/router — initializing TestWorkspace concurrently from
// multiple tests races on global state, so all tests share a single env.
fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(ws.gateway_config(), Some(store.clone()));
        SharedEnv {
            _ws: ws,
            store,
            router,
        }
    })
}

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "tl-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

fn timeline_event(
    root: &str,
    event_type: &str,
    altitude: &str,
    created_at: &str,
) -> LiveDigestEventRecord {
    LiveDigestEventRecord {
        event_id: format!("ev-{}", uuid::Uuid::new_v4()),
        root_session_id: root.to_string(),
        source_session_id: root.to_string(),
        turn_id: Some("turn-1".to_string()),
        source_agent_id: Some("planner.default".to_string()),
        source_node_id: "gateway".to_string(),
        event_type: event_type.to_string(),
        payload: Some(serde_json::json!({ "k": "v" }).to_string()),
        // Deterministic timestamps so ordering never falls back to random UUIDs.
        created_at: created_at.to_string(),
        principal_kind: Some("autonoetic_agent".to_string()),
        principal_id: Some("planner.default".to_string()),
        role: Some("planner".to_string()),
        altitude: Some(altitude.to_string()),
        refs_json: None,
    }
}

#[tokio::test]
async fn session_timeline_list_returns_seeded_events_above_floor() {
    let env = shared();
    // Distinct root keeps this test isolated from others sharing the store.
    let root = "root-tl-floor";
    env.store
        .create_live_digest_event(&timeline_event(
            root,
            "approval.pending",
            "attention",
            "2026-06-01T10:00:00+00:00",
        ))
        .unwrap();
    env.store
        .create_live_digest_event(&timeline_event(
            root,
            "turn.start",
            "detail",
            "2026-06-01T10:00:01+00:00",
        ))
        .unwrap();

    // Omit min_altitude → defaults to `normal` (the type contract).
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "session.timeline.list",
            serde_json::json!({ "root_session_id": root, "limit": 50 }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let entries = result["entries"].as_array().expect("entries array");
    // Only the Attention event clears the default (normal) floor; Detail is filtered.
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event_type"], "approval.pending");
    assert_eq!(entries[0]["principal"]["kind"], "autonoetic_agent");
    assert_eq!(entries[0]["role"]["role"], "planner");

    // Dropping the floor to `detail` surfaces both.
    let resp_all = env
        .router
        .dispatch(make_jsonrpc(
            "session.timeline.list",
            serde_json::json!({ "root_session_id": root, "min_altitude": "detail", "limit": 50 }),
        ))
        .await;
    let all = resp_all.result.expect("result");
    assert_eq!(all["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn session_timeline_list_requires_root_session_id() {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "session.timeline.list",
            serde_json::json!({ "root_session_id": "  " }),
        ))
        .await;
    let err = resp.error.expect("missing root_session_id must error");
    assert_eq!(err.code, -32602);
}

#[tokio::test]
async fn clone_timeline_for_fork_mirrors_history_up_to_turn() {
    let env = shared();
    let src = "root-fork-src";

    // Build a source timeline spanning four turns, with an interleaved
    // turn-less operator message between turns 1 and 2.
    let mk = |turn: Option<&str>, ts: &str, etype: &str| LiveDigestEventRecord {
        event_id: format!("ev-{}", uuid::Uuid::new_v4()),
        root_session_id: src.to_string(),
        source_session_id: src.to_string(),
        turn_id: turn.map(|t| t.to_string()),
        source_agent_id: Some("planner.default".to_string()),
        source_node_id: "gateway".to_string(),
        event_type: etype.to_string(),
        payload: Some("{}".to_string()),
        created_at: ts.to_string(),
        principal_kind: Some("autonoetic_agent".to_string()),
        principal_id: Some("planner.default".to_string()),
        role: Some("planner".to_string()),
        altitude: Some("normal".to_string()),
        refs_json: None,
    };
    for ev in [
        mk(Some("turn-000001"), "2026-06-01T10:00:00+00:00", "turn.start"),
        mk(None, "2026-06-01T10:00:01+00:00", "operator.message"),
        mk(Some("turn-000002"), "2026-06-01T10:00:02+00:00", "turn.start"),
        mk(Some("turn-000003"), "2026-06-01T10:00:03+00:00", "turn.start"),
        mk(Some("turn-000004"), "2026-06-01T10:00:04+00:00", "turn.start"),
    ] {
        env.store.create_live_digest_event(&ev).unwrap();
    }

    // Fork at turn 2: turns 1–2 plus the turn-less message before the cutoff
    // should be mirrored; turns 3 and 4 must not.
    let fork_root = "root-fork-branch";
    let copied = env.store.clone_timeline_for_fork(src, fork_root, 2).unwrap();
    assert_eq!(copied, 3, "turns 1, 2 and the interleaved op message");

    let result = env
        .store
        .list_session_timeline(fork_root, None, 100, None, None)
        .unwrap();
    assert_eq!(result.entries.len(), 3);
    let turns: Vec<Option<String>> = result.entries.iter().map(|e| e.turn_id.clone()).collect();
    assert!(turns.contains(&Some("turn-000001".to_string())));
    assert!(turns.contains(&Some("turn-000002".to_string())));
    assert!(turns.contains(&None), "turn-less operator message mirrored");
    assert!(!turns.contains(&Some("turn-000003".to_string())));
    assert!(!turns.contains(&Some("turn-000004".to_string())));
    assert!(result.entries.iter().all(|e| e.root_session_id == fork_root));
}

#[tokio::test]
async fn session_timeline_list_rejects_invalid_min_altitude() {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "session.timeline.list",
            serde_json::json!({ "root_session_id": "root-x", "min_altitude": "bogus" }),
        ))
        .await;
    let err = resp.error.expect("invalid min_altitude must error, not silently disable filtering");
    assert_eq!(err.code, -32602);
}
