//! Smoke-test install gate.
//!
//! New agents can be required to execute once (via agent_spawn with revision_id)
//! before promotion. This proves the agent actually runs under real conditions,
//! not just that mocked unit tests pass in a no-network sandbox.
//!
//! Tests:
//!   - new agent + required smoke test + no evidence → blocked
//!   - new agent + required smoke test + passed task → promoted
//!   - new agent + required smoke test + failed task → blocked
//!   - new agent + skip mode + no evidence → promoted
//!   - existing agent re-promote + required mode → no smoke-test requirement

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{
    AgentInstallSmokeTestMode, GatewayConfig,
};
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use std::sync::Arc;
use tempfile::tempdir;

const AGENT_ID: &str = "smoke-test-agent";
const REVISION_ID: &str = "rev_smoke_candidate";
const OUTGOING_REVISION: &str = "rev_outgoing";

fn manifest_with_revision_cap(agent_id: &str) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn skill_md(agent_id: &str) -> String {
    format!(
        "---\nversion: \"1.0\"\nruntime:\n  engine: autonoetic\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: stateful\n  sandbox: bubblewrap\n  runtime_lock: runtime.lock\nagent:\n  id: {}\n  name: {}\n  description: test\n---\n# Test\n",
        agent_id, agent_id,
    )
}

fn write_revision_skill(
    gateway_dir: &std::path::Path,
    agent_id: &str,
    revision_id: &str,
) {
    let dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), skill_md(agent_id)).unwrap();
}

fn make_revision_record(agent_id: &str, revision_id: &str, status: AgentRevisionStatus) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", revision_id),
        runtime_lock_hash: "sha256:lock".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status,
        metadata_json: serde_json::Value::Null,
        short_id: revision_id.chars().take(8).collect(),
        signature: None,
        signer_id: None,
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    config: GatewayConfig,
}

fn setup_harness(smoke_test_mode: AgentInstallSmokeTestMode, existing_alias: bool) -> Harness {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_dir = agents_dir.join(AGENT_ID);
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), skill_md(AGENT_ID)).unwrap();

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    write_revision_skill(&gateway_dir, AGENT_ID, REVISION_ID);
    store
        .insert_agent_revision(&make_revision_record(
            AGENT_ID,
            REVISION_ID,
            AgentRevisionStatus::Candidate,
        ))
        .unwrap();

    if existing_alias {
        write_revision_skill(&gateway_dir, AGENT_ID, OUTGOING_REVISION);
        store
            .insert_agent_revision(&make_revision_record(
                AGENT_ID,
                OUTGOING_REVISION,
                AgentRevisionStatus::Ready,
            ))
            .unwrap();
        let alias = AgentAliasRecord {
            alias_id: AGENT_ID.to_string(),
            agent_id: AGENT_ID.to_string(),
            revision_id: OUTGOING_REVISION.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "test".to_string(),
            reason: None,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        };
        store.upsert_agent_alias(&alias).unwrap();
    }

    let mut config = GatewayConfig::default();
    config.agents_dir = agents_dir.clone();
    config.sentinel.enabled = false;
    config.require_operator_approval_for_new_agents = false;
    config.agent_install_smoke_test = smoke_test_mode;

    Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
        config,
    }
}

fn create_smoke_test_task(
    h: &Harness,
    task_id: &str,
    status: TaskRunStatus,
) -> String {
    let workflow = autonoetic_gateway::scheduler::workflow_store::ensure_workflow_for_root_session(
        &h.config,
        Some(h.store.as_ref()),
        "root-smoke-test",
        None,
    )
    .unwrap();

    let mut run = workflow.clone();
    run.status = WorkflowRunStatus::Active;
    autonoetic_gateway::scheduler::workflow_store::save_workflow_run(&h.config, Some(h.store.as_ref()), &run)
        .unwrap();

    let task = TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow.workflow_id.clone(),
        agent_id: AGENT_ID.to_string(),
        session_id: format!("{}/{}-test", workflow.root_session_id, AGENT_ID),
        parent_session_id: workflow.root_session_id.clone(),
        status,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("agent-factory.default".to_string()),
        result_summary: None,
        join_group: None,
        message: Some("smoke test".to_string()),
        metadata: Some(serde_json::json!({
            "_autonoetic_spawn_revision_id": REVISION_ID,
        })),
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    autonoetic_gateway::scheduler::workflow_store::save_task_run(
        &h.config,
        Some(h.store.as_ref()),
        &task,
    )
    .unwrap();

    workflow.workflow_id
}

fn invoke_promote(
    h: &Harness,
    smoke_test_workflow_id: Option<&str>,
    smoke_test_task_id: Option<&str>,
) -> serde_json::Value {
    let manifest = manifest_with_revision_cap(AGENT_ID);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let mut args = serde_json::json!({
        "agent_id": AGENT_ID,
        "revision_id": REVISION_ID,
    });
    if let Some(wid) = smoke_test_workflow_id {
        args["smoke_test_workflow_id"] = serde_json::json!(wid);
    }
    if let Some(tid) = smoke_test_task_id {
        args["smoke_test_task_id"] = serde_json::json!(tid);
    }
    let raw = registry
        .execute(
            "agent_revision_promote",
            &manifest,
            &policy,
            &h.agent_dir,
            Some(&h.gateway_dir),
            &args.to_string(),
            Some("test-session"),
            Some("turn-000001"),
            Some(&h.config),
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn required_mode_blocks_new_agent_without_smoke_test() {
    let h = setup_harness(AgentInstallSmokeTestMode::Required, false);
    let result = invoke_promote(&h, None, None);

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "smoke_test_required");
}

#[test]
fn required_mode_promotes_new_agent_with_passed_smoke_test() {
    let h = setup_harness(AgentInstallSmokeTestMode::Required, false);
    let wf_id = create_smoke_test_task(&h, "smoke-pass-001", TaskRunStatus::Succeeded);
    let result = invoke_promote(&h, Some(&wf_id), Some("smoke-pass-001"));

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
    assert_eq!(result["installed"], true);
}

#[test]
fn required_mode_blocks_new_agent_with_failed_smoke_test() {
    let h = setup_harness(AgentInstallSmokeTestMode::Required, false);
    let wf_id = create_smoke_test_task(&h, "smoke-fail-001", TaskRunStatus::Failed);
    let result = invoke_promote(&h, Some(&wf_id), Some("smoke-fail-001"));

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "smoke_test_failed_or_mismatched");
}

#[test]
fn skip_mode_promotes_new_agent_without_smoke_test() {
    let h = setup_harness(AgentInstallSmokeTestMode::Skip, false);
    let result = invoke_promote(&h, None, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn required_mode_exempts_existing_agent_repromote() {
    let h = setup_harness(AgentInstallSmokeTestMode::Required, true);
    let result = invoke_promote(&h, None, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}
