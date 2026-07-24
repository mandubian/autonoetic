//! `curation.run_for_session` JSON-RPC — operator-triggered memory curation on
//! a specific session, with optional focus notes.
//!
//! Exercises the full router dispatch path (resolve curator -> enqueue into the
//! session's workflow -> append curation.triggered event), mirroring how the
//! `/curate` TUI command reaches the gateway.

mod support;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::principal::PrincipalKind;
use support::TestWorkspace;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const CURATOR_ID: &str = "memory-curator.default";
const CURATOR_REV: &str =
    "rev_sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "curate-test".to_string(),
        method: method.to_string(),
        params,
        auth_token: None,
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&dest)?;
            copy_dir_all(&path, &dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

/// Write a minimal SKILL.md for the curator so the repository can load it.
fn write_curator_bundle(agents_dir: &Path) -> anyhow::Result<PathBuf> {
    let agent_dir = agents_dir.join(CURATOR_ID);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        "---\n\
         version: \"1.0\"\n\
         runtime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\n\
         agent:\n  id: \"memory-curator.default\"\n  name: \"Memory Curator\"\n  description: \"test curator\"\n  singleton: true\n\
         ---\n# Memory Curator (test stub)\n",
    )?;
    Ok(agent_dir)
}

/// Register a revision mirror so `resolve_target_to_agent_ref` resolves the
/// curator to a concrete revision. Mirrors the helper in
/// background_scheduler_integration.
fn register_curator_revision(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    authoring_dir: &Path,
) -> anyhow::Result<()> {
    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(CURATOR_ID)
        .join(CURATOR_REV);
    std::fs::create_dir_all(&rev_dir)?;
    copy_dir_all(authoring_dir, &rev_dir)?;

    let rec = AgentRevisionRecord {
        revision_id: CURATOR_REV.to_string(),
        agent_id: CURATOR_ID.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{CURATOR_ID}"),
        runtime_lock_hash: "sha256:cur-test".to_string(),
        manifest_hash: "sha256:cur-manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "curation_integration".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec)?;
    let alias = AgentAliasRecord {
        alias_id: CURATOR_ID.to_string(),
        agent_id: CURATOR_ID.to_string(),
        revision_id: CURATOR_REV.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "curation_integration".to_string(),
        reason: Some("integration test seed".to_string()),
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias)?;
    Ok(())
}

struct Env {
    _ws: TestWorkspace,
    store: Arc<GatewayStore>,
    router: JsonRpcRouter,
}

// JsonRpcRouter::new initializes the global constitution runtime, which cannot
// be re-initialized with a different path in the same process. All tests that
// dispatch through the router therefore share one workspace (the curator is
// registered once). The "missing bundle" case is tested without a router.
static SHARED: OnceLock<Env> = OnceLock::new();

fn shared() -> &'static Env {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let authoring_dir = write_curator_bundle(&ws.agents_dir).expect("curator bundle");
        let config = ws.gateway_config();
        register_curator_revision(&config, &store, &authoring_dir).expect("curator revision");
        let router = JsonRpcRouter::new(config, Some(store.clone()));
        Env {
            _ws: ws,
            store,
            router,
        }
    })
}

#[tokio::test]
async fn curate_with_focus_notes_enqueues_curator_on_session_workflow() -> anyhow::Result<()> {
    let env = shared();
    let root = "root-curate-with-notes";

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "curation.run_for_session",
            serde_json::json!({
                "root_session_id": root,
                "focus_notes": "weight the retry loop, looks like a missing approval",
            }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let task_id = result.get("task_id").and_then(|v| v.as_str()).expect("task_id");
    let workflow_id = result
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .expect("workflow_id");
    assert_eq!(result.get("session_id").and_then(|v| v.as_str()), Some(root));

    // The task was enqueued in the session's own workflow.
    let queued = env.store.list_queued_tasks_for_workflow(workflow_id)?;
    assert_eq!(queued.len(), 1, "exactly one queued task");
    assert_eq!(queued[0].task_id, task_id);
    assert_eq!(queued[0].agent_id, format!("{CURATOR_ID}@{CURATOR_REV}"));
    assert_eq!(queued[0].parent_session_id, root);

    // The message is the JSON the curator's Input section parses, carrying the
    // session_ids, max_sessions, and the operator's focus_notes.
    let msg: serde_json::Value = serde_json::from_str(&queued[0].message)?;
    assert_eq!(
        msg["session_ids"].as_array().and_then(|a| a.first()).and_then(|v| v.as_str()),
        Some(root)
    );
    assert_eq!(msg["max_sessions"].as_u64(), Some(50));
    assert_eq!(
        msg["focus_notes"].as_str(),
        Some("weight the retry loop, looks like a missing approval")
    );

    // A curation.triggered event was recorded.
    let events = env.store.list_workflow_events(workflow_id)?;
    let triggered = events
        .iter()
        .find(|e| e.event_type == "curation.triggered")
        .expect("curation.triggered event appended");
    assert_eq!(triggered.task_id.as_deref(), Some(task_id));
    assert_eq!(
        triggered.payload.get("manual").and_then(|v| v.as_bool()),
        Some(true)
    );

    Ok(())
}

#[tokio::test]
async fn curate_without_notes_carries_null_focus() -> anyhow::Result<()> {
    let env = shared();
    let root = "root-curate-no-notes";

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "curation.run_for_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let workflow_id = result
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .expect("workflow_id");

    let queued = env.store.list_queued_tasks_for_workflow(workflow_id)?;
    assert_eq!(queued.len(), 1);
    let msg: serde_json::Value = serde_json::from_str(&queued[0].message)?;
    // focus_notes is explicitly null when no notes were provided, so the agent
    // can treat the field uniformly and skip Step 1b.
    assert!(msg.get("focus_notes").is_some(), "focus_notes key present");
    assert!(msg["focus_notes"].is_null());

    Ok(())
}

#[tokio::test]
async fn curate_fails_when_curator_bundle_missing() -> anyhow::Result<()> {
    // A workspace with NO curator revision registered. We exercise the
    // resolution step directly (the first thing the handler does) rather than
    // building a router, because JsonRpcRouter::new initializes the global
    // constitution runtime, which cannot be re-pointed at a second workspace
    // in the same process. The handler wraps exactly this call and surfaces its
    // error verbatim, so this validates the operator-facing failure.
    let ws = TestWorkspace::new()?;
    let store = GatewayStore::open(ws.path())?;

    let result = autonoetic_gateway::runtime::tools::resolve_target_to_agent_ref(
        "memory-curator.default",
        &store,
    );

    let err = result.expect_err("expected resolution to fail with no bundle");
    assert!(
        err.to_string().contains("memory-curator.default"),
        "error should name the curator: {err}"
    );

    Ok(())
}
