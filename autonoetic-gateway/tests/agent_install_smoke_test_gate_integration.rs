//! Smoke-test install gate (#578).
//!
//! Capability-bearing new agents must execute once before promotion.
//! Pure-reasoning agents (no NetworkAccess / CodeExecution) are exempt.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use autonoetic_types::promotion::{Finding, PromotionRole};
use std::sync::Arc;
use tempfile::tempdir;

const AGENT_ID: &str = "smoke-test-agent";
const REVISION_ID: &str = "rev_smoke_candidate";
const OUTGOING_REVISION: &str = "rev_outgoing";
const ARTIFACT_ID: &str = "art_smoke_test01";

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
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn skill_md(agent_id: &str, executable: bool, with_credentials: bool) -> String {
    if !executable {
        return format!(
            "---\nname: \"{agent_id}\"\ndescription: test\nmetadata:\n  autonoetic:\n    version: \"1.0\"\n    runtime:\n      engine: autonoetic\n      gateway_version: \"0.1.0\"\n      sdk_version: \"0.1.0\"\n      type: stateful\n      sandbox: bubblewrap\n      runtime_lock: runtime.lock\n    agent:\n      id: {agent_id}\n      name: {agent_id}\n      description: test\n---\n# Test\n",
            agent_id = agent_id,
        );
    }
    let caps = if with_credentials {
            r#"    capabilities:
      - type: "CredentialAccess"
        services: ["trading-api"]
      - type: "NetworkAccess"
        hosts: ["api.example.com"]
      - type: "WriteAccess"
        scopes: ["self.*"]"#
    } else {
        r#"    capabilities:
      - type: "NetworkAccess"
        hosts: ["api.open-meteo.com"]"#
    };
    format!(
        "---\nname: \"{agent_id}\"\ndescription: test\nmetadata:\n  autonoetic:\n    version: \"1.0\"\n    runtime:\n      engine: autonoetic\n      gateway_version: \"0.1.0\"\n      sdk_version: \"0.1.0\"\n      type: stateful\n      sandbox: bubblewrap\n      runtime_lock: runtime.lock\n    agent:\n      id: {agent_id}\n      name: {agent_id}\n      description: test\n{caps}\n---\n# Test\n",
        agent_id = agent_id,
        caps = caps,
    )
}

fn write_revision_skill(
    gateway_dir: &std::path::Path,
    agent_id: &str,
    revision_id: &str,
    executable: bool,
    with_credentials: bool,
) {
    let dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        skill_md(agent_id, executable, with_credentials),
    )
    .unwrap();
    if with_credentials {
        let lock = r#"gateway:
  artifact: autonoetic-gateway
  version: "0.1.0"
  sha256: sha256:test
sdk:
  version: "0.1.0"
sandbox:
  backend: bubblewrap
credentials:
  - service: trading-api
"#;
        std::fs::write(dir.join("runtime.lock"), lock).unwrap();
    }
}

fn make_revision_record(
    agent_id: &str,
    revision_id: &str,
    status: AgentRevisionStatus,
    with_artifact: bool,
) -> AgentRevisionRecord {
    AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: with_artifact.then(|| ARTIFACT_ID.to_string()),
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
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    }
}

fn seed_promotion_records(gateway_dir: &std::path::Path, content_digest: &str) {
    let store =
        autonoetic_gateway::runtime::promotion_store::PromotionStore::new(gateway_dir).unwrap();
    store
        .record_promotion(
            ARTIFACT_ID.to_string(),
            None,
            Some(content_digest.to_string()),
            PromotionRole::Evaluator,
            "evaluator.default",
            true,
            Vec::<Finding>::new(),
            None,
        )
        .unwrap();
    store
        .record_promotion(
            ARTIFACT_ID.to_string(),
            None,
            Some(content_digest.to_string()),
            PromotionRole::Auditor,
            "auditor.default",
            true,
            Vec::<Finding>::new(),
            None,
        )
        .unwrap();
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    config: GatewayConfig,
}

fn setup_harness(executable: bool, with_credentials: bool, existing_alias: bool) -> Harness {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_dir = agents_dir.join(AGENT_ID);
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("SKILL.md"),
        skill_md(AGENT_ID, executable, with_credentials),
    )
    .unwrap();

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    write_revision_skill(&gateway_dir, AGENT_ID, REVISION_ID, executable, with_credentials);
    let content_digest = format!("sha256:{}", REVISION_ID);
    store
        .insert_agent_revision(&make_revision_record(
            AGENT_ID,
            REVISION_ID,
            AgentRevisionStatus::Candidate,
            executable,
        ))
        .unwrap();
    if executable {
        seed_promotion_records(&gateway_dir, &content_digest);
    }

    if existing_alias {
        write_revision_skill(&gateway_dir, AGENT_ID, OUTGOING_REVISION, executable, false);
        store
            .insert_agent_revision(&make_revision_record(
                AGENT_ID,
                OUTGOING_REVISION,
                AgentRevisionStatus::Ready,
                executable,
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
    message: Option<&str>,
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
    autonoetic_gateway::scheduler::workflow_store::save_workflow_run(
        &h.config,
        Some(h.store.as_ref()),
        &run,
    )
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
        message: message.map(|m| m.to_string()),
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
    smoke_test_input: Option<&str>,
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
    if let Some(input) = smoke_test_input {
        args["smoke_test_input"] = serde_json::json!(input);
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
fn capability_bearing_new_agent_blocks_without_smoke_test() {
    let h = setup_harness(true, false, false);
    let result = invoke_promote(&h, None, None, None);

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "smoke_test_required");
}

#[test]
fn capability_bearing_new_agent_promotes_with_passed_smoke_test() {
    let h = setup_harness(true, false, false);
    let wf_id = create_smoke_test_task(&h, "smoke-pass-001", TaskRunStatus::Succeeded, None);
    let result = invoke_promote(&h, Some(&wf_id), Some("smoke-pass-001"), None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
    assert_eq!(result["installed"], true);
}

#[test]
fn capability_bearing_new_agent_blocks_with_failed_smoke_test() {
    let h = setup_harness(true, false, false);
    let wf_id = create_smoke_test_task(&h, "smoke-fail-001", TaskRunStatus::Failed, None);
    let result = invoke_promote(&h, Some(&wf_id), Some("smoke-fail-001"), None);

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "smoke_test_failed_or_mismatched");
}

#[test]
fn pure_reasoning_new_agent_promotes_without_smoke_test() {
    let h = setup_harness(false, false, false);
    let result = invoke_promote(&h, None, None, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn existing_agent_repromote_exempts_smoke_test() {
    let h = setup_harness(true, false, true);
    let result = invoke_promote(&h, None, None, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn operator_directed_requires_smoke_test_input() {
    let h = setup_harness(true, true, false);
    let wf_id = create_smoke_test_task(
        &h,
        "smoke-cred-001",
        TaskRunStatus::Succeeded,
        Some("buy 1 share"),
    );
    let result = invoke_promote(&h, Some(&wf_id), Some("smoke-cred-001"), None);

    assert_eq!(result["ok"], false, "unexpected: {:?}", result);
    assert_eq!(result["error"], "smoke_test_input_required");
}

#[test]
fn operator_directed_promotes_with_matching_input() {
    let h = setup_harness(true, true, false);
    let input = "buy 1 share of AAPL";
    let wf_id = create_smoke_test_task(
        &h,
        "smoke-cred-002",
        TaskRunStatus::Succeeded,
        Some(input),
    );
    let result = invoke_promote(
        &h,
        Some(&wf_id),
        Some("smoke-cred-002"),
        Some(input),
    );

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn legacy_skip_config_does_not_bypass_capability_gate() {
    let mut h = setup_harness(true, false, false);
    h.config.agent_install_smoke_test =
        autonoetic_types::config::AgentInstallSmokeTestMode::Skip;
    let result = invoke_promote(&h, None, None, None);

    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "smoke_test_required");
}
