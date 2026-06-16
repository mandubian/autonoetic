//! Constitution R++3 — Distinct evaluator/auditor identity at promotion.
//!
//! The evaluator and auditor backing a promotion must be distinct agent
//! identities (not merely distinct sessions of the same agent). A single
//! agent that self-approves by recording both evaluator and auditor passes
//! must be rejected.

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

    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", main_py.as_bytes()),
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
    artifact_id: &str,
    role: &str,
    pass: bool,
    session_id: &str,
) {
    let args = serde_json::json!({
        "artifact_id": artifact_id,
        "role": role,
        "pass": pass,
        "findings": [],
        "summary": format!("{role} check — pass={pass}"),
    });
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
            None,
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
/// so the `unwrap_err` failure assertions below read naturally.
fn as_outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) if v["ok"] == serde_json::Value::Bool(false) => {
            Err(v["message"].as_str().unwrap_or_default().to_string())
        }
        other => other,
    }
}

#[test]
fn same_agent_identity_rejected_even_if_both_passed() {
    let agent_id = "rpp3.test.agent";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), &skill);
    let config = GatewayConfig {
        agents_dir,
        // Isolate the distinct-identity completeness gate from the new-agent
        // first-admission human gate (covered by its own test).
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

    // Same agent identity (evaluator.default) records both evaluator and auditor passes.
    let same_agent = manifest_for("sealed_evaluator.default");
    let same_policy = PolicyEngine::new(same_agent.clone());

    record_promotion(
        &registry,
        &same_agent,
        &same_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-eval",
    );
    record_promotion(
        &registry,
        &same_agent,
        &same_policy,
        &builder_dir,
        &gateway_dir,
        &config,
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

    assert!(
        as_outcome(result.clone()).is_err(),
        "promote should fail when evaluator and auditor share identity"
    );
    let err = as_outcome(result).unwrap_err();
    assert!(
        err.contains("P-2.17"),
        "error should reference P-2.17: {err}"
    );
    assert!(
        err.contains("same agent"),
        "error should mention same agent: {err}"
    );
    assert!(
        err.contains("sealed_evaluator.default"),
        "error should name the overlapping agent: {err}"
    );
}

#[test]
fn distinct_identities_allowed() {
    let agent_id = "rpp3.distinct.agent";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), &skill);
    let config = GatewayConfig {
        agents_dir,
        // Isolate the distinct-identity completeness gate from the new-agent
        // first-admission human gate (covered by its own test).
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

    let evaluator = manifest_for("sealed_evaluator.default");
    let eval_policy = PolicyEngine::new(evaluator.clone());
    record_promotion(
        &registry,
        &evaluator,
        &eval_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-eval",
    );

    let auditor = manifest_for("auditor.default");
    let audit_policy = PolicyEngine::new(auditor.clone());
    record_promotion(
        &registry,
        &auditor,
        &audit_policy,
        &builder_dir,
        &gateway_dir,
        &config,
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

    assert!(
        result.is_ok(),
        "promote should succeed with distinct identities, got err: {:?}",
        as_outcome(result).unwrap_err()
    );
    let promoted = result.unwrap();
    assert_eq!(promoted["ok"], true);
}
