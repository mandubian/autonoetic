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
use autonoetic_types::artifact::{ArtifactKind, ArtifactRefRecord, ArtifactRefScopeType};
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

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    let registry = default_registry();

    let eval_args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-eval-test",
    );

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
            Some(gw_store.clone()),
            None,
        )
        .expect("evaluator promotion.record should succeed");

    let eval_parsed: serde_json::Value = serde_json::from_str(&eval_result).unwrap();
    assert_eq!(eval_parsed.get("ok").and_then(|v| v.as_bool()), Some(true));

    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());

    let audit_args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        &artifact_id,
        "auditor",
        true,
        "session-audit-test",
    );

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
            Some(gw_store.clone()),
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

    let (smoke_wf, smoke_task) = support::promotion_trace::seed_smoke_test_task(
        &config,
        gw_store.as_ref(),
        "promotion.test.agent",
        revision_id,
    );
    let promote_args = serde_json::json!({
        "agent_id": "promotion.test.agent",
        "revision_id": revision_id,
        "reason": "integration test promote after promotion records",
        "smoke_test_workflow_id": smoke_wf,
        "smoke_test_task_id": smoke_task,
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

/// Promotion flow using artifact_ref (ar.*) instead of artifact_id.
#[tokio::test]
async fn test_promotion_record_with_artifact_ref() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");

    let script_content = "#!/usr/bin/env python3\nprint('ar test')\n";
    let (artifact_id, gateway_dir) = build_agent_bundle_artifact(temp.path(), script_content);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store"));

    // Inspect the artifact to get digest fields for the ref record.
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let bundle = artifact_store.inspect(&artifact_id).unwrap();

    // Mint an ar.* ref manually and insert into the store.
    // Use root-session scope so all child sessions can resolve it.
    let ar_ref = "ar.promoe2etest01";
    gw_store
        .create_artifact_ref(&ArtifactRefRecord {
            ref_id: ar_ref.to_string(),
            scope_type: ArtifactRefScopeType::Session,
            scope_id: "root-session-ar".to_string(),
            artifact_id: artifact_id.clone(),
            artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
            artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
            created_by_agent_id: "coder.default".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            expires_at: None,
            revoked_at: None,
        })
        .expect("create artifact ref");

    // --- Evaluator records pass via artifact_ref ---
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    let registry = default_registry();

    let mut eval_args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        &artifact_id,
        "sealed_evaluator",
        true,
        "root-session-ar/evaluator",
    );
    eval_args.as_object_mut().unwrap().remove("artifact_id");
    eval_args["artifact_ref"] = serde_json::json!(ar_ref);

    let eval_result = registry
        .execute(
            "promotion_record",
            &eval_manifest,
            &eval_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&eval_args).unwrap(),
            Some("root-session-ar/evaluator"),
            None,
            Some(&config),
            Some(gw_store.clone()),
            None,
        )
        .expect("evaluator promotion.record via ar.* ref should succeed");

    let eval_parsed: serde_json::Value = serde_json::from_str(&eval_result).unwrap();
    assert_eq!(eval_parsed.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        eval_parsed
            .pointer("/promotion_record/artifact_ref")
            .and_then(|v| v.as_str()),
        Some(ar_ref),
        "response should include the artifact_ref"
    );

    // --- Auditor records pass via artifact_ref ---
    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());

    let audit_args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        &artifact_id,
        "auditor",
        true,
        "root-session-ar/auditor",
    );
    let mut audit_args = audit_args;
    audit_args.as_object_mut().unwrap().remove("artifact_id");
    audit_args["artifact_ref"] = serde_json::json!(ar_ref);

    let audit_result = registry
        .execute(
            "promotion_record",
            &audit_manifest,
            &audit_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&audit_args).unwrap(),
            Some("root-session-ar/auditor"),
            None,
            Some(&config),
            Some(gw_store.clone()),
            None,
        )
        .expect("auditor promotion.record via ar.* ref should succeed");

    let audit_parsed: serde_json::Value = serde_json::from_str(&audit_result).unwrap();
    assert_eq!(audit_parsed.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        audit_parsed
            .pointer("/promotion_record/artifact_ref")
            .and_then(|v| v.as_str()),
        Some(ar_ref),
        "response should include the artifact_ref"
    );

    // Verify promotion store recorded for the canonical artifact_id.
    let promotion_store = PromotionStore::new(&gateway_dir).expect("promotion store");
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
        "artifact should be fully promoted"
    );

    // --- promotion_query via artifact_ref ---
    let query_manifest = AgentManifest {
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
            id: "reader.default".to_string(),
            name: "reader".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
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
    };
    let query_policy = PolicyEngine::new(query_manifest.clone());

    let query_args = serde_json::json!({
        "artifact_ref": ar_ref,
    });
    let query_result = registry
        .execute(
            "promotion_query",
            &query_manifest,
            &query_policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&query_args).unwrap(),
            Some("root-session-ar/query"),
            None,
            Some(&config),
            Some(gw_store),
            None,
        )
        .expect("promotion_query via ar.* ref should succeed");

    let query_parsed: serde_json::Value = serde_json::from_str(&query_result).unwrap();
    assert_eq!(
        query_parsed
            .pointer("/auditor_pass")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        query_parsed
            .get("artifact_ref")
            .and_then(|v| v.as_str()),
        Some(ar_ref),
        "promotion_query should return the artifact_ref"
    );
}

/// Bless-on-promotion (determinism inc 3): a passing verdict on an artifact with
/// a dependency layer freezes that layer's resolved closure onto the promotion
/// record and surfaces it in the response.
#[tokio::test]
async fn test_promotion_blesses_resolved_closure() {
    use autonoetic_gateway::layer_store::LayerStore;
    use autonoetic_types::layer::ArtifactLayer;

    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    // A dependency layer carrying build-time resolved-version provenance.
    let deps_src = temp.path().join("deps");
    std::fs::create_dir_all(deps_src.join("requests-2.31.0.dist-info")).unwrap();
    let layer = LayerStore::new(&gateway_dir, Default::default())
        .unwrap()
        .create_from_dir(&deps_src, "python-deps", "/opt/autonoetic-deps", None)
        .unwrap();
    assert!(!layer.resolved_packages.is_empty(), "layer should have provenance");

    // Build an AgentBundle artifact referencing that layer.
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "bless-session";
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
  id: "bless.test.agent"
  name: "Bless Test"
  description: "x"
capabilities: []
execution_mode: script
script_entry: main.py
---
# Bless Test
"#;
    // runtime.lock consistent with the artifact's layer (the fixture references it).
    let runtime_lock = format!(
        "gateway:\n  artifact: autonoetic-gateway\n  version: \"0.1.0\"\n  sha256: unmanaged\n  signature: null\nsdk:\n  version: \"0.1.0\"\nsandbox:\n  backend: bubblewrap\ndependencies: []\nartifacts: []\nlayers:\n  - layer_id: {}\n    digest: {}\n    mount_path: /opt/autonoetic-deps\n",
        layer.layer_id, layer.digest
    );
    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", b"print(1)\n".as_slice()),
    ] {
        let h = content_store.write(content).unwrap();
        content_store.register_name(session_id, path, &h).unwrap();
    }
    let bundle = artifact_store
        .build_with_kind(
            &[
                "SKILL.md".to_string(),
                "runtime.lock".to_string(),
                "main.py".to_string(),
            ],
            Some(&["main.py".to_string()]),
            Some(&[ArtifactLayer {
                layer_id: layer.layer_id.clone(),
                name: "python-deps".to_string(),
                mount_path: "/opt/autonoetic-deps".to_string(),
                digest: layer.digest.clone(),
            }]),
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    // Record a passing promotion via the tool.
    let config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    let manifest = evaluator_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gw_store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let args = support::promotion_trace::build_promotion_record_args(
        gw_store.as_ref(),
        &bundle.artifact_id,
        "sealed_evaluator",
        true,
        session_id,
    );
    let result = registry
        .execute(
            "promotion_record",
            &manifest,
            &policy,
            &builder_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some(session_id),
            None,
            Some(&config),
            Some(gw_store),
            None,
        )
        .expect("promotion.record should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    let blessed = parsed
        .pointer("/promotion_record/blessed_packages")
        .and_then(|v| v.as_array())
        .expect("blessed_packages should be present in the response");
    assert!(
        blessed.iter().any(|p| {
            p.get("name").and_then(|n| n.as_str()) == Some("requests")
                && p.get("version").and_then(|v| v.as_str()) == Some("2.31.0")
        }),
        "blessed closure should include requests==2.31.0, got: {blessed:?}"
    );

    // And it's persisted on the promotion record.
    let store = PromotionStore::new(&gateway_dir).unwrap();
    assert!(!store
        .get_promotion(&bundle.artifact_id)
        .unwrap()
        .blessed_packages
        .is_empty());
}
