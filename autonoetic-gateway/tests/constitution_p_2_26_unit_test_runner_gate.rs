//! Constitution P-2.26 — All executed gate roles must pass.
//!
//! When a unit_test_runner has recorded a verdict for a revision's artifact,
//! the promotion gate mechanically checks that unit_test_runner_pass is true.
//! A failed unit test run must block promotion even if evaluator and auditor
//! both passed.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;

fn high_risk_skill_md(agent_id: &str) -> String {
    format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{agent_id}"
  name: "{agent_id}"
  description: "High-risk test agent"
capabilities:
  - type: CodeExecution
    patterns: ["python3 "]
execution_mode: script
script_entry: main.py
---
# Test agent
"#
    )
}

fn manifest_for(agent_id: &str) -> AgentManifest {
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
            description: "Test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

fn build_agent_bundle(base_dir: &Path, skill_md: &str) -> (String, PathBuf) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "test-session";

    let runtime_lock = r#"gateway:
  artifact: autonoetic-gateway
  version: "0.1.0"
  sha256: unmanaged
  signature: null
sdk:
  version: "0.1.0"
sandbox:
  backend: bubblewrap
dependencies: []
artifacts: []
layers: []
"#;

    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";
    let test_py = "#!/usr/bin/env python3\nimport unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n";

    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", main_py.as_bytes()),
        ("test_main.py", test_py.as_bytes()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, path, &handle)
            .unwrap();
    }

    let bundle = artifact_store
        .build_with_kind(
            &[
                "SKILL.md".to_string(),
                "runtime.lock".to_string(),
                "main.py".to_string(),
                "test_main.py".to_string(),
            ],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();
    (bundle.artifact_id, gateway_dir)
}

fn record_promotion(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    builder_dir: &Path,
    gateway_dir: &Path,
    config: &GatewayConfig,
    gw_store: &Arc<GatewayStore>,
    artifact_id: &str,
    role: &str,
    pass: bool,
    session_id: &str,
) {
    let args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        artifact_id,
        role,
        pass,
        session_id,
    );
    let result = registry
        .execute(
            "promotion_record",
            manifest,
            policy,
            builder_dir,
            Some(gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some(session_id),
            None,
            Some(config),
            Some(gw_store.clone()),
            None,
        )
        .expect("promotion.record should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promotion.record should return ok=true, got: {result}"
    );
}

fn create_revision(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    builder_dir: &Path,
    gateway_dir: &Path,
    config: &GatewayConfig,
    store: Arc<GatewayStore>,
    agent_id: &str,
    artifact_id: &str,
) -> String {
    let args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": artifact_id,
    });
    let result = registry
        .execute(
            "agent_revision_create",
            manifest,
            policy,
            builder_dir,
            Some(gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-create"),
            None,
            Some(config),
            Some(store),
            None,
        )
        .expect("revision create should succeed");

    let created: serde_json::Value = serde_json::from_str(&result).unwrap();
    created
        .get("revision_id")
        .and_then(|v| v.as_str())
        .expect("revision_id in response")
        .to_string()
}

fn try_promote(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    builder_dir: &Path,
    gateway_dir: &Path,
    config: &GatewayConfig,
    store: Arc<GatewayStore>,
    agent_id: &str,
    revision_id: &str,
) -> Result<serde_json::Value, String> {
    let args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "integration test",
    });
    match registry.execute(
        "agent_revision_promote",
        manifest,
        policy,
        builder_dir,
        Some(gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("session-promote"),
        None,
        Some(config),
        Some(store),
        None,
    ) {
        Ok(result) => Ok(serde_json::from_str(&result).unwrap()),
        Err(e) => Err(e.to_string()),
    }
}

/// Re-present a structured P-5.11 gate *block* (`Ok(ok:false)`) as `Err(message)`
/// so the failure assertions below read naturally; a genuine success stays `Ok`.
fn as_outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) if v["ok"] == serde_json::Value::Bool(false) => {
            Err(v["message"].as_str().unwrap_or_default().to_string())
        }
        other => other,
    }
}

#[test]
fn promotion_blocked_when_unit_test_runner_failed() {
    let agent_id = "p226.test.agent";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), &skill);
    let config = GatewayConfig {
        agents_dir,
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();

    let builder = manifest_for("specialized_builder.default");
    let builder_policy = PolicyEngine::new(builder.clone());

    let revision_id = create_revision(
        &registry,
        &builder,
        &builder_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store.clone(),
        agent_id,
        &artifact_id,
    );

    // static_evaluator passes
    let eval_agent = manifest_for("sealed_evaluator.default");
    let eval_policy = PolicyEngine::new(eval_agent.clone());
    record_promotion(
        &registry,
        &eval_agent,
        &eval_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &store,
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-sealed-eval",
    );

    // unit_test_runner FAILS
    let test_agent = manifest_for("unit_test_runner.default");
    let test_policy = PolicyEngine::new(test_agent.clone());
    record_promotion(
        &registry,
        &test_agent,
        &test_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &store,
        &artifact_id,
        "unit_test_runner",
        false,
        "session-unit-test",
    );

    // auditor passes
    let audit_agent = manifest_for("auditor.default");
    let audit_policy = PolicyEngine::new(audit_agent.clone());
    record_promotion(
        &registry,
        &audit_agent,
        &audit_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-audit",
    );

    // Promotion must be refused despite evaluator and auditor passing.
    let result = try_promote(
        &registry,
        &builder,
        &builder_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(as_outcome(result.clone()).is_err(), "promotion should be blocked when unit_test_runner failed");
    let err = as_outcome(result).unwrap_err();
    assert!(
        err.contains("P-2.26") || err.contains("unit_test_runner"),
        "error message should mention P-2.26 or unit_test_runner, got: {err}"
    );
}

#[test]
fn promotion_succeeds_when_no_unit_test_runner_ran() {
    let agent_id = "p226-no-runner.test.agent";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), &skill);
    let config = GatewayConfig {
        agents_dir,
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();

    let builder = manifest_for("specialized_builder.default");
    let builder_policy = PolicyEngine::new(builder.clone());

    let revision_id = create_revision(
        &registry,
        &builder,
        &builder_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store.clone(),
        agent_id,
        &artifact_id,
    );

    let eval_agent = manifest_for("sealed_evaluator.default");
    let eval_policy = PolicyEngine::new(eval_agent.clone());
    record_promotion(
        &registry,
        &eval_agent,
        &eval_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &store,
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-sealed-eval",
    );

    // No unit_test_runner recorded at all.

    let audit_agent = manifest_for("auditor.default");
    let audit_policy = PolicyEngine::new(audit_agent.clone());
    record_promotion(
        &registry,
        &audit_agent,
        &audit_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-audit",
    );

    let result = try_promote(
        &registry,
        &builder,
        &builder_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(result.is_ok(), "promotion should succeed when unit_test_runner never ran, got error: {:?}", result.err());
}
