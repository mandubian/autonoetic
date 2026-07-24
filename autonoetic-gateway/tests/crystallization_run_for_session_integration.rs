//! `skill.crystallize_from_session` JSON-RPC — operator-triggered skill
//! crystallization on a specific session (#818), with optional notes naming the
//! tactic.
//!
//! Exercises the full router dispatch path (resolve crystallizer -> take the
//! singleton slot -> enqueue into the session's workflow -> append
//! crystallization.triggered), mirroring how `/crystallize` in the session room
//! reaches the gateway.

mod support;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::principal::PrincipalKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use support::TestWorkspace;

const CRYSTALLIZER_ID: &str = "skill-crystallizer.default";
const CRYSTALLIZER_REV: &str =
    "rev_sha256:5555555555555555555555555555555555555555555555555555555555555555";

fn make_jsonrpc(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: "crystallize-test".to_string(),
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

/// Minimal crystallizer bundle. `singleton: true` matches the reference bundle —
/// the dedup assertion below depends on it.
fn write_crystallizer_bundle(agents_dir: &Path) -> anyhow::Result<PathBuf> {
    let agent_dir = agents_dir.join(CRYSTALLIZER_ID);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        "---\n\
         version: \"1.0\"\n\
         runtime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\n\
         agent:\n  id: \"skill-crystallizer.default\"\n  name: \"Skill Crystallizer\"\n  description: \"test crystallizer\"\n  singleton: true\n\
         ---\n# Skill Crystallizer (test stub)\n",
    )?;
    Ok(agent_dir)
}

fn register_crystallizer_revision(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    authoring_dir: &Path,
) -> anyhow::Result<()> {
    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(CRYSTALLIZER_ID)
        .join(CRYSTALLIZER_REV);
    std::fs::create_dir_all(&rev_dir)?;
    copy_dir_all(authoring_dir, &rev_dir)?;

    let rec = AgentRevisionRecord {
        revision_id: CRYSTALLIZER_REV.to_string(),
        agent_id: CRYSTALLIZER_ID.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{CRYSTALLIZER_ID}"),
        runtime_lock_hash: "sha256:crys-test".to_string(),
        manifest_hash: "sha256:crys-manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "crystallization_integration".to_string(),
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
        alias_id: CRYSTALLIZER_ID.to_string(),
        agent_id: CRYSTALLIZER_ID.to_string(),
        revision_id: CRYSTALLIZER_REV.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "crystallization_integration".to_string(),
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
    config: autonoetic_types::config::GatewayConfig,
}

// JsonRpcRouter::new initializes the global constitution runtime, which cannot
// be re-pointed at a second workspace in the same process, so every
// router-dispatching test shares one workspace. The "missing bundle" case is
// tested without a router.
static SHARED: OnceLock<Env> = OnceLock::new();

fn shared() -> &'static Env {
    SHARED.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let authoring_dir = write_crystallizer_bundle(&ws.agents_dir).expect("crystallizer bundle");
        let config = ws.gateway_config();
        register_crystallizer_revision(&config, &store, &authoring_dir)
            .expect("crystallizer revision");
        let router = JsonRpcRouter::new(config.clone(), Some(store.clone()));
        Env {
            _ws: ws,
            store,
            router,
            config,
        }
    })
}

#[tokio::test]
async fn crystallize_with_notes_enqueues_crystallizer_on_session_workflow() -> anyhow::Result<()> {
    let env = shared();
    let root = "root-crystallize-with-notes";

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({
                "root_session_id": root,
                "focus_notes": "the retry-with-backoff around the flaky API",
            }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp.result.expect("result");
    let task_id = result
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id");
    let workflow_id = result
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .expect("workflow_id");
    assert_eq!(
        result.get("session_id").and_then(|v| v.as_str()),
        Some(root)
    );

    // Enqueued into the session's own workflow, so the verdict lands in the
    // timeline the operator is watching.
    let queued = env.store.list_queued_tasks_for_workflow(workflow_id)?;
    assert_eq!(queued.len(), 1, "exactly one queued task");
    assert_eq!(queued[0].task_id, task_id);
    assert_eq!(
        queued[0].agent_id,
        format!("{CRYSTALLIZER_ID}@{CRYSTALLIZER_REV}")
    );
    assert_eq!(queued[0].parent_session_id, root);
    // Operator-initiated, not agent-initiated.
    assert_eq!(queued[0].source_agent_id, "operator");

    // The message is the JSON the crystallizer's Input section parses.
    let msg: serde_json::Value = serde_json::from_str(&queued[0].message)?;
    assert_eq!(
        msg["session_ids"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str()),
        Some(root)
    );
    assert_eq!(
        msg["focus_notes"].as_str(),
        Some("the retry-with-backoff around the flaky API")
    );

    let events = env.store.list_workflow_events(workflow_id)?;
    let triggered = events
        .iter()
        .find(|e| e.event_type == "crystallization.triggered")
        .expect("crystallization.triggered event appended");
    assert_eq!(triggered.task_id.as_deref(), Some(task_id));
    assert_eq!(
        triggered.payload.get("manual").and_then(|v| v.as_bool()),
        Some(true)
    );

    Ok(())
}

#[tokio::test]
async fn crystallize_without_notes_carries_null_focus() -> anyhow::Result<()> {
    let env = shared();
    let root = "root-crystallize-no-notes";

    let resp = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let workflow_id = resp
        .result
        .expect("result")
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("workflow_id");

    let queued = env.store.list_queued_tasks_for_workflow(&workflow_id)?;
    assert_eq!(queued.len(), 1);
    let msg: serde_json::Value = serde_json::from_str(&queued[0].message)?;
    // Present-and-null, so the agent reads the field uniformly instead of
    // branching on absence.
    assert!(msg.get("focus_notes").is_some(), "focus_notes key present");
    assert!(msg["focus_notes"].is_null());

    Ok(())
}

/// The crystallizer is a singleton: a second `/crystallize` before the first
/// run finishes must return the in-flight task, not queue a second one. Without
/// the slot acquisition in the handler, an impatient operator would fire two
/// crystallizers at the same session and get two competing proposals.
#[tokio::test]
async fn repeated_crystallize_deduplicates_to_the_running_task() -> anyhow::Result<()> {
    let env = shared();
    let root = "root-crystallize-dedup";

    let first = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;
    assert!(first.error.is_none(), "unexpected error: {:?}", first.error);
    let first_result = first.result.expect("result");
    let first_task = first_result
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id")
        .to_string();
    let workflow_id = first_result
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .expect("workflow_id")
        .to_string();

    let second = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;
    assert!(
        second.error.is_none(),
        "unexpected error: {:?}",
        second.error
    );
    let second_result = second.result.expect("result");
    assert_eq!(
        second_result.get("status").and_then(|v| v.as_str()),
        Some("deduplicated"),
        "second call should report the in-flight run"
    );
    assert_eq!(
        second_result.get("task_id").and_then(|v| v.as_str()),
        Some(first_task.as_str()),
        "dedup should name the task already running"
    );

    // And no second task was enqueued.
    let queued = env.store.list_queued_tasks_for_workflow(&workflow_id)?;
    assert_eq!(queued.len(), 1, "still exactly one queued task");

    Ok(())
}

/// A singleton slot taken for a task that never got queued would make every
/// later `/crystallize` in that workflow dedup to a phantom run, wedging the
/// command until the workflow is cleaned up. The failure is reachable today:
/// `enqueue_task` refuses on an emergency-stopped workflow, which is exactly the
/// state an operator who just hit `/estop` is in.
#[tokio::test]
async fn enqueue_failure_releases_the_singleton_slot() -> anyhow::Result<()> {
    use autonoetic_types::workflow::WorkflowRunStatus;

    let env = shared();
    let root = "root-crystallize-estop";
    let config = &env.config;

    // Materialize the workflow, then put it in emergency stop so enqueue refuses.
    let workflow = autonoetic_gateway::scheduler::ensure_workflow_for_root_session(
        config,
        Some(env.store.as_ref()),
        root,
        Some(CRYSTALLIZER_ID),
    )?;
    let mut run = autonoetic_gateway::scheduler::load_workflow_run(
        config,
        Some(env.store.as_ref()),
        &workflow.workflow_id,
    )?
    .expect("workflow run");
    run.status = WorkflowRunStatus::EmergencyStopped;
    autonoetic_gateway::scheduler::save_workflow_run(config, Some(env.store.as_ref()), &run)?;

    let blocked = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;
    assert!(
        blocked.error.is_some(),
        "enqueue should fail on an emergency-stopped workflow, got {:?}",
        blocked.result
    );

    // Lift the stop — the operator's next /crystallize must actually run rather
    // than dedup to the task that failed to queue.
    run.status = WorkflowRunStatus::Active;
    autonoetic_gateway::scheduler::save_workflow_run(config, Some(env.store.as_ref()), &run)?;

    let retry = env
        .router
        .dispatch(make_jsonrpc(
            "skill.crystallize_from_session",
            serde_json::json!({ "root_session_id": root }),
        ))
        .await;
    assert!(retry.error.is_none(), "unexpected error: {:?}", retry.error);
    let result = retry.result.expect("result");
    assert_ne!(
        result.get("status").and_then(|v| v.as_str()),
        Some("deduplicated"),
        "the slot from the failed enqueue must not survive as a phantom run"
    );
    // The retry is really queued, under its own id.
    let retry_task_id = result
        .get("task_id")
        .and_then(|v| v.as_str())
        .expect("task_id");
    let queued = env
        .store
        .list_queued_tasks_for_workflow(&workflow.workflow_id)?;
    assert!(
        queued.iter().any(|t| t.task_id == retry_task_id),
        "retry task should be queued, got {:?}",
        queued.iter().map(|t| &t.task_id).collect::<Vec<_>>()
    );
    // NOTE: the refused enqueue also left an orphan row behind — `enqueue_task`
    // commits the queue row before its emergency-stop check (#883). That is not
    // asserted as correct here; this test owns the slot-release invariant, and
    // the count is left unpinned so fixing #883 does not break it.
    assert!(
        queued.len() <= 2,
        "expected at most the retry plus the #883 orphan, got {}",
        queued.len()
    );

    Ok(())
}

/// The reference bundle must parse and must carry the properties the rest of the
/// system depends on: the id the router resolves, the singleton flag its dedup
/// relies on, spawn power to delegate, and *no* revision power — the crystallizer
/// proposes and routes, it never installs (P-9.15).
#[test]
fn reference_bundle_declares_a_proposer_not_an_installer() -> anyhow::Result<()> {
    use autonoetic_types::capability::Capability;

    let skill_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("agents")
        .join("evolution")
        .join(CRYSTALLIZER_ID)
        .join("SKILL.md");
    let content = std::fs::read_to_string(&skill_path)
        .unwrap_or_else(|e| panic!("reference bundle {} should read: {e}", skill_path.display()));
    let (manifest, instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&content)
            .expect("reference bundle should parse");

    assert_eq!(manifest.agent.id, CRYSTALLIZER_ID);
    assert!(
        manifest.agent.singleton,
        "the router's /crystallize dedup takes a singleton slot; the bundle must declare one"
    );
    assert!(
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::AgentSpawn { .. })),
        "must be able to delegate enactment"
    );
    assert!(
        !manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::AgentRevision { .. })),
        "must NOT hold AgentRevision — enactment goes through the factory/builder one door"
    );

    // The output contract the operator surface and the router's callers read.
    let returns = manifest
        .io
        .as_ref()
        .and_then(|io| io.returns.as_ref())
        .expect("io.returns declared");
    let required: Vec<&str> = returns
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    for field in ["verdict", "rationale", "proposal_id"] {
        assert!(
            required.contains(&field),
            "io.returns should require '{field}', got {required:?}"
        );
    }

    // The three routes it may take are named in the instructions, so a rename of
    // an enactor cannot silently leave the SKILL pointing at a dead agent.
    for enactor in [
        "evolution-steward.default",
        "agent-adapter.default",
        "agent-factory.default",
    ] {
        assert!(
            instructions.contains(enactor),
            "instructions should name the enactor '{enactor}'"
        );
    }

    Ok(())
}

#[tokio::test]
async fn crystallize_fails_when_bundle_missing() -> anyhow::Result<()> {
    // Workspace with NO crystallizer revision registered. The resolution step is
    // exercised directly (the first thing the handler does) because
    // JsonRpcRouter::new cannot be pointed at a second workspace in-process; the
    // handler surfaces this error verbatim, so this is the operator-facing
    // failure.
    let ws = TestWorkspace::new()?;
    let store = GatewayStore::open(ws.path())?;

    let result = autonoetic_gateway::runtime::tools::resolve_target_to_agent_ref(
        "skill-crystallizer.default",
        &store,
    );

    let err = result.expect_err("expected resolution to fail with no bundle");
    assert!(
        err.to_string().contains("skill-crystallizer.default"),
        "error should name the crystallizer: {err}"
    );

    Ok(())
}
