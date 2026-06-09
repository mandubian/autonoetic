//! Integration tests for the install pipeline hardening promotion gates.
//!
//! Tests the mechanical guardrails from spec-install-pipeline-hardening.md § 3.1:
//! - High-risk agents (NetworkAccess/CodeExecution/AgentSpawn) require both
//!   evaluator and auditor pass records before promotion.
//! - High-risk agents with unresolved dependencies are blocked from promotion.
//! - Low-risk agents are not subject to the promotion gate.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::promotion::PromotionRole;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;

// ── Helpers ────────────────────────────────────────────────────

/// Builds an agent bundle artifact with customizable SKILL.md content.
fn build_agent_bundle(base_dir: &Path, skill_md: &str, main_py: &str) -> (String, PathBuf) {
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
  description: "High-risk test agent with NetworkAccess"
capabilities:
  - type: NetworkAccess
    hosts:
      - "api.example.com"
execution_mode: script
script_entry: main.py
---
# High-risk test agent
"#
    )
}

fn low_risk_skill_md(agent_id: &str) -> String {
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
  description: "Low-risk test agent with no high-risk capabilities"
capabilities: []
execution_mode: script
script_entry: main.py
---
# Low-risk test agent
"#
    )
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
            description: "Sealed Evaluator".to_string(),
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

/// Creates a revision via `agent.revision.create` and returns the revision_id.
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
    let create_args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": artifact_id,
    });
    let create_result = registry
        .execute(
            "agent_revision_create",
            manifest,
            policy,
            builder_dir,
            Some(gateway_dir),
            &serde_json::to_string(&create_args).unwrap(),
            Some("session-create"),
            None,
            Some(config),
            Some(store),
            None,
        )
        .expect("revision create should succeed");

    let created: serde_json::Value = serde_json::from_str(&create_result).unwrap();
    created
        .get("revision_id")
        .and_then(|v| v.as_str())
        .expect("revision_id in response")
        .to_string()
}

/// Records a promotion via `promotion.record`.
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
            Some(&format!("session-{role}")),
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

/// Attempts `agent.revision.promote` and returns Ok(json) or Err(message).
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
    let promote_args = serde_json::json!({
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
        &serde_json::to_string(&promote_args).unwrap(),
        Some("session-promote"),
        None,
        Some(config),
        Some(store),
        None,
    ) {
        Ok(result) => {
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            Ok(parsed)
        }
        Err(e) => Err(e.to_string()),
    }
}

struct TestSetup {
    _temp: tempfile::TempDir,
    #[allow(dead_code)]
    agents_dir: PathBuf,
    gateway_dir: PathBuf,
    builder_dir: PathBuf,
    config: GatewayConfig,
    registry: autonoetic_gateway::runtime::tools::NativeToolRegistry,
    b_manifest: AgentManifest,
    b_policy: PolicyEngine,
}

fn setup_test(agent_id: &str, skill_md: &str) -> (TestSetup, String, String, Arc<GatewayStore>) {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");

    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";
    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), skill_md, main_py);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        // This suite hardens the audit/eval/identity promotion gates; isolate it
        // from the new-agent first-admission human gate (its own tests).
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let b_manifest = builder_manifest();
    let b_policy = PolicyEngine::new(b_manifest.clone());

    let revision_id = create_revision(
        &registry,
        &b_manifest,
        &b_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store.clone(),
        agent_id,
        &artifact_id,
    );

    let setup = TestSetup {
        _temp: temp,
        agents_dir,
        gateway_dir,
        builder_dir,
        config,
        registry,
        b_manifest,
        b_policy,
    };

    (setup, artifact_id, revision_id, store)
}

// ── Tests ──────────────────────────────────────────────────────

/// § 3.1 — High-risk agent promotion fails when no promotion records exist.
#[test]
fn test_promote_rejects_high_risk_without_promotion_records() {
    let agent_id = "hr.no.records";
    let skill = high_risk_skill_md(agent_id);
    let (s, _artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(
        result.is_err(),
        "promote should fail without promotion records"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("Promotion gate") && err.contains("no promotion.record"),
        "error should mention promotion gate: {err}"
    );
}

/// § 3.1 — High-risk agent promotion succeeds when both evaluator and auditor passed.
#[test]
fn test_promote_succeeds_with_both_evaluator_and_auditor_pass() {
    let agent_id = "hr.both.pass";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "sealed_evaluator",
        true,
    );

    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());
    record_promotion(
        &s.registry,
        &audit_manifest,
        &audit_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "auditor",
        true,
    );

    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(result.is_ok(), "promote should succeed: {:?}", result.err());
    let json = result.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted")
    );
}

/// § 3.1 — Promotion fails when evaluator passed but auditor did not pass.
#[test]
fn test_promote_rejects_when_evaluator_fails() {
    let agent_id = "hr.eval.fail";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    // evaluator fails
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "sealed_evaluator",
        false,
    );

    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());
    record_promotion(
        &s.registry,
        &audit_manifest,
        &audit_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "auditor",
        true,
    );

    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(result.is_err(), "promote should fail when evaluator fails");
    let err = result.unwrap_err();
    assert!(
        err.contains("no evaluator role passed"),
        "error should mention evaluator failure: {err}"
    );
}

/// § 3.1 — Promotion fails when evaluator passed but auditor record is missing.
#[test]
fn test_promote_rejects_when_auditor_missing() {
    let agent_id = "hr.auditor.missing";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    // Only evaluator records — no auditor
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "sealed_evaluator",
        true,
    );

    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(
        result.is_err(),
        "promote should fail when auditor is missing"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("auditor did not pass"),
        "error should mention auditor failure: {err}"
    );
}

/// § 3.1 — Low-risk agents skip the promotion gate entirely.
#[test]
fn test_promote_allows_low_risk_without_records() {
    let agent_id = "lr.no.records";
    let skill = low_risk_skill_md(agent_id);
    let (s, _artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    // No promotion records at all — should still succeed for low-risk agent
    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );

    assert!(
        result.is_ok(),
        "low-risk agent should promote without records: {:?}",
        result.err()
    );
    let json = result.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
}

/// § 3.1 + § 3.2 — High-risk agent with unresolved dependencies is blocked
/// even when both evaluator and auditor passed.
#[test]
fn test_promote_rejects_high_risk_with_unresolved_dependencies() {
    let agent_id = "hr.unresolved.deps";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        // This suite hardens the audit/eval/identity promotion gates; isolate it
        // from the new-agent first-admission human gate (its own tests).
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };

    // Manually create the revision directory with the high-risk SKILL.md
    let revision_id = "rev_sha256:test_unresolved_deps_001";
    let revision_dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&revision_dir).unwrap();
    std::fs::write(revision_dir.join("SKILL.md"), &skill).unwrap();

    // Insert a revision record with has_unresolved_dependencies=true and an artifact_id
    let artifact_id = "art_unresolved_test";
    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.to_string()),
        content_digest: "sha256:test_unresolved".to_string(),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test_harness".to_string(),
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({
            "has_unresolved_dependencies": true,
            "dependency_files": ["requirements.txt"],
            "detected_external_imports": ["requests"],
        }),
        short_id: String::new(),
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();

    // Record both evaluator and auditor pass — so the only remaining blocker
    // is the unresolved dependencies flag.
    let registry = default_registry();
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &registry,
        &eval_manifest,
        &eval_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        artifact_id,
        "sealed_evaluator",
        true,
    );

    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());
    record_promotion(
        &registry,
        &audit_manifest,
        &audit_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        artifact_id,
        "auditor",
        true,
    );

    let b_manifest = builder_manifest();
    let b_policy = PolicyEngine::new(b_manifest.clone());
    let result = try_promote(
        &registry,
        &b_manifest,
        &b_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store,
        agent_id,
        revision_id,
    );

    assert!(
        result.is_err(),
        "promote should fail with unresolved dependencies"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("unresolved dependencies"),
        "error should mention unresolved dependencies: {err}"
    );
}

/// § 3.8 — Promotion evidence is keyed by canonical content_digest, not by revision timestamp.
#[test]
fn test_promote_accepts_precreate_records_when_digest_matches() {
    let agent_id = "hr.stale.records";
    let skill = high_risk_skill_md(agent_id);
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).unwrap();

    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        // This suite hardens the audit/eval/identity promotion gates; isolate it
        // from the new-agent first-admission human gate (its own tests).
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };

    // Record promotion records before revision creation.
    let artifact_id = "art_stale_test";
    let promo_store = PromotionStore::new(&gateway_dir).unwrap();
    // record_promotion auto-timestamps "now". We then create a revision with a future
    // created_at to verify timestamp ordering no longer blocks promotion.
    promo_store
        .record_promotion(
            artifact_id.to_string(),
            None,
            None,
            PromotionRole::Evaluator,
            "evaluator.default",
            true,
            vec![],
            Some("eval pass".to_string()),
        )
        .unwrap();
    promo_store
        .record_promotion(
            artifact_id.to_string(),
            None,
            None,
            PromotionRole::Auditor,
            "auditor.default",
            true,
            vec![],
            Some("audit pass".to_string()),
        )
        .unwrap();

    // Create the revision with created_at in the future relative to pre-recorded evidence.
    let revision_id = "rev_sha256:test_stale_freshness_001";
    let revision_dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&revision_dir).unwrap();
    std::fs::write(revision_dir.join("SKILL.md"), &skill).unwrap();

    let future_ts = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.to_string()),
        content_digest: "sha256:test_stale".to_string(),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: future_ts,
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test_harness".to_string(),
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();

    let registry = default_registry();
    let b_manifest = builder_manifest();
    let b_policy = PolicyEngine::new(b_manifest.clone());
    let result = try_promote(
        &registry,
        &b_manifest,
        &b_policy,
        &builder_dir,
        &gateway_dir,
        &config,
        store,
        agent_id,
        revision_id,
    );

    assert!(
        result.is_ok(),
        "promote should accept pre-create promotion records once digest binding matches"
    );
}

/// Full pipeline: high-risk agent goes through create → evaluator → auditor → promote.
#[test]
fn test_full_pipeline_with_builder_and_promotion_gates() {
    let agent_id = "hr.full.pipeline";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) = setup_test(agent_id, &skill);

    // Step 1: Attempt promotion before any records — should fail
    let fail_result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store.clone(),
        agent_id,
        &revision_id,
    );
    assert!(
        fail_result.is_err(),
        "Step 1: promote should fail without records"
    );
    assert!(fail_result.unwrap_err().contains("no promotion.record"));

    // Step 2: Evaluator records pass
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "sealed_evaluator",
        true,
    );

    // Step 3: Attempt promotion with only evaluator — should still fail
    let partial_result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store.clone(),
        agent_id,
        &revision_id,
    );
    assert!(
        partial_result.is_err(),
        "Step 3: promote should fail with only evaluator"
    );
    assert!(partial_result.unwrap_err().contains("auditor did not pass"));

    // Step 4: Auditor records pass
    let audit_manifest = auditor_manifest();
    let audit_policy = PolicyEngine::new(audit_manifest.clone());
    record_promotion(
        &s.registry,
        &audit_manifest,
        &audit_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &artifact_id,
        "auditor",
        true,
    );

    // Step 5: Now promote should succeed
    let success_result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store,
        agent_id,
        &revision_id,
    );
    assert!(
        success_result.is_ok(),
        "Step 5: promote should succeed with both records: {:?}",
        success_result.err()
    );
    let json = success_result.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted")
    );
}
