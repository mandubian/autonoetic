//! End-to-end test for content-linked promotion gate flow.
//!
//! Tests the complete lifecycle:
//! 1. Coder writes content → content_handle = sha256:...
//! 2. Evaluator validates content → calls promotion.record(pass=true)
//! 3. Auditor audits content → calls promotion.record(pass=true)
//! 4. Builder creates an `AgentBundle` artifact and activates it via `agent.revision.create` + `agent.revision.promote`

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::promotion::PromotionRole;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn build_agent_bundle_artifact(base_dir: &Path, main_py: &str) -> (String, std::path::PathBuf) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "test-session";

    let skill_md = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "promotion.test.agent"
  name: "Promotion Test Agent"
  description: "Tests content-linked promotion path"
capabilities: []
execution_mode: script
script_entry: main.py
---
# Promotion Test Agent
"#;

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

fn builder_manifest() -> AgentManifest {
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
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "Builder".to_string(),
        },
        capabilities: vec![
            Capability::AgentSpawn {
                max_children: 10,
                max_spawn_depth: 0,
            },
            Capability::AgentRevision {
                patterns: vec!["*".to_string()],
            },
        ],
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

fn evaluator_manifest() -> AgentManifest {
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
            id: "sealed_evaluator.default".to_string(),
            name: "sealed_evaluator.default".to_string(),
            description: "Evaluator".to_string(),
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: vec!["sandbox.".to_string(), "content.".to_string()],
        }],
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

fn auditor_manifest() -> AgentManifest {
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
            id: "auditor.default".to_string(),
            name: "auditor.default".to_string(),
            description: "Auditor".to_string(),
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: vec!["content.".to_string()],
        }],
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

/// Full promotion flow through revision create + promote (no `agent.install`).
#[tokio::test]
async fn test_promotion_record_full_pass_flow() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");

    let script_content = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'temp': 22}))\n";
    let (artifact_id, gateway_dir) = build_agent_bundle_artifact(temp.path(), script_content);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let content_store = ContentStore::new(&gateway_dir).expect("content store should create");
    let content_handle = content_store
        .write(script_content.as_bytes())
        .expect("content should write");
    assert!(content_handle.starts_with("sha256:"));

    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    let registry = default_registry();

    let eval_args = serde_json::json!({
        "artifact_id": artifact_id,
        "role": "sealed_evaluator",
        "pass": true,
        "findings": [],
        "summary": "All tests passed"
    });

    let eval_result = registry
        .execute(
            "promotion_record",
            &eval_manifest,
            &eval_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&eval_args).unwrap(),
            Some("session-eval-test"),
            None,
            Some(&config),
            None,
            None,
        )
        .expect("evaluator promotion.record should succeed");

    let eval_parsed: serde_json::Value = serde_json::from_str(&eval_result).unwrap();
    assert_eq!(eval_parsed.get("ok").and_then(|v| v.as_bool()), Some(true));

    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());

    let audit_args = serde_json::json!({
        "artifact_id": artifact_id,
        "role": "auditor",
        "pass": true,
        "findings": [],
        "summary": "Security audit passed"
    });

    let audit_result = registry
        .execute(
            "promotion_record",
            &audit_manifest,
            &audit_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&audit_args).unwrap(),
            Some("session-audit-test"),
            None,
            Some(&config),
            None,
            None,
        )
        .expect("auditor promotion.record should succeed");

    let audit_parsed: serde_json::Value = serde_json::from_str(&audit_result).unwrap();
    assert_eq!(audit_parsed.get("ok").and_then(|v| v.as_bool()), Some(true));

    let promotion_store = PromotionStore::new(&gateway_dir).expect("promotion store should create");
    assert!(
        promotion_store.has_passed(&artifact_id, &PromotionRole::SealedEvaluator),
        "evaluator should have passed"
    );
    assert!(
        promotion_store.has_passed(&artifact_id, &PromotionRole::Auditor),
        "auditor should have passed"
    );
    assert!(
        promotion_store.is_fully_promoted(&artifact_id),
        "content should be fully promoted"
    );

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let b_manifest = builder_manifest();
    let b_policy = PolicyEngine::new(b_manifest.clone());

    let create_args = serde_json::json!({
        "agent_id": "promotion.test.agent",
        "artifact_id": artifact_id,
    });
    let create_result = registry
        .execute(
            "agent_revision_create",
            &b_manifest,
            &b_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&create_args).unwrap(),
            Some("session-rev-create"),
            None,
            Some(&config),
            Some(gw_store.clone()),
            None,
        )
        .expect("revision create should succeed");

    let created: serde_json::Value = serde_json::from_str(&create_result).unwrap();
    let revision_id = created
        .get("revision_id")
        .and_then(|v| v.as_str())
        .expect("revision_id in response");

    let promote_args = serde_json::json!({
        "agent_id": "promotion.test.agent",
        "revision_id": revision_id,
        "reason": "integration test promote after promotion records",
    });
    let promote_result = registry
        .execute(
            "agent_revision_promote",
            &b_manifest,
            &b_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&promote_args).unwrap(),
            Some("session-promote"),
            None,
            Some(&config),
            Some(gw_store),
            None,
        )
        .expect("promote should succeed");

    let promoted: serde_json::Value = serde_json::from_str(&promote_result).unwrap();
    assert_eq!(promoted.get("ok").and_then(|v| v.as_bool()), Some(true));

    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("promotion.test.agent")
        .join(revision_id);
    assert!(rev_dir.join("SKILL.md").exists(), "SKILL.md materialized");
    assert!(rev_dir.join("main.py").exists(), "main.py materialized");
}
