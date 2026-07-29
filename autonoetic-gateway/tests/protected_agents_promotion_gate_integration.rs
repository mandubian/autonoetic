//! Protected-agent promotion gate (issue #21).
//!
//! Critical agents (e.g. agent-factory.default) cannot be promoted without
//! eval-run evidence. This closes the recursive-trust loop: a regressed
//! agent-factory cannot silently replace itself.
//!
//! Tests:
//!   - protected agent without eval run → blocked
//!   - protected agent with passed eval run → proceeds (to remaining gates)
//!   - non-protected agent → no extra gate (existing behavior)
//!   - gate disabled → protected agent promotes without eval
//!   - protected agent with failed eval run → blocked by eval gate
//!   - protected agent with eval run for wrong revision → blocked by eval gate

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{GatewayConfig, ProtectedAgentsConfig};
use autonoetic_types::evaluation::{EvalRunRecord, EvalRunStatus};
use autonoetic_types::principal::PrincipalKind;
use std::sync::Arc;
use tempfile::tempdir;
use support::manifest_builder::TestManifest;

const AGENT_ID: &str = "agent-factory.default";
const OTHER_AGENT_ID: &str = "coder.default";
const OUTGOING_REVISION: &str = "rev_outgoing";
const INCOMING_REVISION: &str = "rev_incoming";

fn manifest_with_revision_cap(agent_id: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
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

fn make_revision_record(agent_id: &str, revision_id: &str) -> AgentRevisionRecord {
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
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "artifact".to_string(),
        source_ref: None,
        origin_node_id: "local".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::Value::Null,
        short_id: revision_id.chars().take(8).collect(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    }
}

fn make_eval_run(
    eval_run_id: &str,
    agent_id: &str,
    revision_id: &str,
    status: EvalRunStatus,
) -> EvalRunRecord {
    EvalRunRecord {
        eval_run_id: eval_run_id.to_string(),
        suite_id: "suite-test".to_string(),
        subject_agent_id: agent_id.to_string(),
        subject_revision_id: revision_id.to_string(),
        baseline_revision_id: None,
        status,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: serde_json::json!({}),
        report_handle: None,
        origin_node_id: "local".to_string(),
    }
}

struct PromoteHarness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    config: GatewayConfig,
}

fn setup_harness(agent_id: &str) -> PromoteHarness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join(agent_id);
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));

    write_revision_skill(&gateway_dir, agent_id, OUTGOING_REVISION);
    write_revision_skill(&gateway_dir, agent_id, INCOMING_REVISION);

    store
        .insert_agent_revision(&make_revision_record(agent_id, OUTGOING_REVISION))
        .unwrap();
    store
        .insert_agent_revision(&make_revision_record(agent_id, INCOMING_REVISION))
        .unwrap();

    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
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

    let mut config = GatewayConfig::default();
    config.agents_dir = agents_dir.clone();
    config.sentinel.enabled = false;
    config.protected_agents = ProtectedAgentsConfig {
        enabled: true,
        agents: vec![AGENT_ID.to_string()],
    };

    PromoteHarness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
        config,
    }
}

fn invoke_promote_raw(
    h: &PromoteHarness,
    agent_id: &str,
    revision_id: &str,
    eval_run_id: Option<&str>,
) -> Result<String, String> {
    let manifest = manifest_with_revision_cap(agent_id);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let mut args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
    });
    if let Some(eval_id) = eval_run_id {
        args["required_eval_run_id"] = serde_json::json!(eval_id);
    }
    match registry.execute(
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
    ) {
        Ok(raw) => Ok(raw),
        Err(e) => Err(e.to_string()),
    }
}

fn invoke_promote(
    h: &PromoteHarness,
    agent_id: &str,
    revision_id: &str,
    eval_run_id: Option<&str>,
) -> serde_json::Value {
    let raw = invoke_promote_raw(h, agent_id, revision_id, eval_run_id)
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn protected_agent_without_eval_run_is_blocked() {
    let h = setup_harness(AGENT_ID);
    let result = invoke_promote(&h, AGENT_ID, INCOMING_REVISION, None);

    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "protected_agent_requires_eval_run");
    assert_eq!(result["protected_agent"], AGENT_ID);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("protected"),
        "{}",
        result
    );
}

#[test]
fn protected_agent_with_passed_eval_run_proceeds() {
    let h = setup_harness(AGENT_ID);

    let eval_run = make_eval_run(
        "eval-passed-001",
        AGENT_ID,
        INCOMING_REVISION,
        EvalRunStatus::Passed,
    );
    h.store.insert_eval_run(&eval_run).unwrap();

    let result = invoke_promote(&h, AGENT_ID, INCOMING_REVISION, Some("eval-passed-001"));

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
    // Terminal signals so the orchestrator goes straight to spawning the agent
    // instead of looping to "confirm" the install.
    assert_eq!(result["installed"], true, "promote success must signal installed: {result:?}");
    assert!(
        result["next"].as_str().is_some_and(|n| n.contains("agent_spawn")),
        "promote success must tell the orchestrator to spawn the agent: {result:?}"
    );
}

#[test]
fn non_protected_agent_not_gated() {
    let h = setup_harness(OTHER_AGENT_ID);
    let mut config = GatewayConfig::default();
    config.agents_dir = h._temp.path().join("agents");
    config.sentinel.enabled = false;
    config.protected_agents = ProtectedAgentsConfig {
        enabled: true,
        agents: vec![AGENT_ID.to_string()],
    };
    let h = PromoteHarness {
        config,
        ..h
    };

    let result = invoke_promote(&h, OTHER_AGENT_ID, INCOMING_REVISION, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn gate_disabled_allows_promotion_without_eval() {
    let h = setup_harness(AGENT_ID);
    let mut config = GatewayConfig::default();
    config.agents_dir = h._temp.path().join("agents");
    config.sentinel.enabled = false;
    config.protected_agents = ProtectedAgentsConfig {
        enabled: false,
        agents: vec![AGENT_ID.to_string()],
    };
    let h = PromoteHarness {
        config,
        ..h
    };

    let result = invoke_promote(&h, AGENT_ID, INCOMING_REVISION, None);

    assert_eq!(result["ok"], true, "unexpected: {:?}", result);
    assert_eq!(result["status"], "promoted");
}

#[test]
fn protected_agent_with_failed_eval_run_is_blocked() {
    let h = setup_harness(AGENT_ID);

    let eval_run = make_eval_run(
        "eval-failed-001",
        AGENT_ID,
        INCOMING_REVISION,
        EvalRunStatus::Failed,
    );
    h.store.insert_eval_run(&eval_run).unwrap();

    let result = invoke_promote_raw(&h, AGENT_ID, INCOMING_REVISION, Some("eval-failed-001"));
    let err = result.expect_err("failed eval run should error");
    assert!(
        err.contains("did not pass"),
        "{}",
        err
    );
}

#[test]
fn protected_agent_with_eval_run_for_wrong_revision_is_blocked() {
    let h = setup_harness(AGENT_ID);

    let eval_run = make_eval_run(
        "eval-wrong-rev-001",
        AGENT_ID,
        OUTGOING_REVISION,
        EvalRunStatus::Passed,
    );
    h.store.insert_eval_run(&eval_run).unwrap();

    let result = invoke_promote_raw(&h, AGENT_ID, INCOMING_REVISION, Some("eval-wrong-rev-001"));
    let err = result.expect_err("wrong revision eval run should error");
    assert!(
        err.contains("was for revision"),
        "{}",
        err
    );
}
