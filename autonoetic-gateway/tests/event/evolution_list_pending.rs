//! `evolution.list_pending` JSON-RPC — the `/skills` standing view (#818).
//!
//! The assembly itself is unit-tested in `evolution_view`; this exercises the
//! router path an operator's `/skills` actually travels, and the one thing only
//! the full stack can show: that a Candidate revision held by the promotion gate
//! is surfaced against the proposal that produced it.


use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use autonoetic_types::principal::PrincipalKind;
use std::sync::{Arc, OnceLock};
use crate::support::TestWorkspace;

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "skills-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

fn put_knowledge(
    store: &GatewayStore,
    id: &str,
    scope: &str,
    tags: &[&str],
    content: serde_json::Value,
) {
    let mut m = MemoryObject::new(
        id.to_string(),
        scope.to_string(),
        "skill-crystallizer.default".to_string(),
        "skill-crystallizer.default".to_string(),
        "session:test:io.returns".to_string(),
        content.to_string(),
    );
    m.source_type = MemorySourceType::AgentWrite;
    m.visibility = MemoryVisibility::Global;
    m.tags = tags.iter().map(|t| t.to_string()).collect();
    store.memory_upsert(&m).expect("knowledge upsert");
}

/// A Candidate revision — what the promotion gate holds while it waits for the
/// operator to acknowledge the capability set.
fn put_candidate_revision(store: &GatewayStore, agent_id: &str, revision_id: &str) {
    let rec = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{revision_id}"),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::AutonoeticAgent.tag().to_string(),
        created_by_id: "specialized_builder.default".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec).expect("revision insert");
}

struct Env {
    _ws: TestWorkspace,
    store: Arc<GatewayStore>,
    router: JsonRpcRouter,
}

// JsonRpcRouter::new initializes the global constitution runtime, which cannot be
// re-pointed at a second workspace in-process, so the router tests share one.
static SHARED: OnceLock<Env> = OnceLock::new();

fn shared() -> &'static Env {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let router = JsonRpcRouter::new(ws.gateway_config(), Some(store.clone()));
        Env {
            _ws: ws,
            store,
            router,
        }
    })
}

#[tokio::test]
async fn list_pending_surfaces_proposals_decisions_and_candidates() -> anyhow::Result<()> {
    let env = shared();

    // A crystallization routed to an existing agent, judged and landed.
    put_knowledge(
        &env.store,
        "crys-landed",
        "evolution/crystallizations",
        &["type:crystallization_verdict", "verdict:graduate"],
        serde_json::json!({
            "verdict": "graduate",
            "rationale": "coder.default already does this; it just is not told to",
            "tactic": { "title": "seal the workbench before running main.py" },
            "target_agent": "coder.default"
        }),
    );
    put_knowledge(
        &env.store,
        "steward.graduation.crys-landed",
        "evolution",
        &["lesson_graduation"],
        serde_json::json!({ "status": "landed", "target_agent": "coder.default" }),
    );

    // A crystallization that minted a new skill, still sitting at the gate.
    put_knowledge(
        &env.store,
        "crys-awaiting",
        "evolution/crystallizations",
        &["type:crystallization_verdict", "verdict:crystallize"],
        serde_json::json!({
            "verdict": "crystallize",
            "rationale": "nothing installed covers the procedure",
            "tactic": { "title": "probe, back off on 429, cache token, then batch" },
            "target_agent": "batch-fetcher.default"
        }),
    );
    put_candidate_revision(
        &env.store,
        "batch-fetcher.default",
        "rev_sha256:1111111111111111111111111111111111111111111111111111111111111111",
    );

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "evolution.list_pending",
            serde_json::json!({ "limit": 30 }),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let rows = result["pending"].as_array().cloned().unwrap_or_default();

    let landed = rows
        .iter()
        .find(|r| r["id"] == "crys-landed")
        .expect("landed crystallization listed");
    assert_eq!(landed["stage"], "judged");
    assert_eq!(landed["outcome"], "landed");
    assert_eq!(landed["target_agent"], "coder.default");

    let awaiting = rows
        .iter()
        .find(|r| r["id"] == "crys-awaiting")
        .expect("awaiting crystallization listed");
    assert_eq!(awaiting["stage"], "proposed");
    // The point of the view: a Candidate the gate is holding shows up against the
    // agent it targets, so an operator can see what is waiting on them. (All of the
    // agent's Candidates — revisions carry no proposal id yet.)
    let candidates = awaiting["target_agent_candidates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(candidates.len(), 1, "candidate revision surfaced");
    assert!(candidates[0]
        .as_str()
        .unwrap_or_default()
        .starts_with("rev_sha256:1111"));
    assert!(
        result["counts"]["awaiting_promotion"].as_u64().unwrap_or(0) >= 1,
        "counts should report work awaiting promotion, got {:?}",
        result["counts"]
    );

    Ok(())
}

/// The `limit` parameter is clamped rather than trusted: a caller asking for
/// 10_000 rows should get a bounded answer, not a scan of the whole store.
#[tokio::test]
async fn list_pending_clamps_an_absurd_limit() -> anyhow::Result<()> {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "evolution.list_pending",
            serde_json::json!({ "limit": 10_000 }),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    assert!(result["pending"].is_array());
    Ok(())
}

/// No params at all is the `/skills` case — the default limit applies.
#[tokio::test]
async fn list_pending_defaults_without_params() -> anyhow::Result<()> {
    let env = shared();
    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "evolution.list_pending",
            serde_json::json!({}),
        ))
        .await;
    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    assert!(resp.result.expect("result")["pending"].is_array());
    Ok(())
}
