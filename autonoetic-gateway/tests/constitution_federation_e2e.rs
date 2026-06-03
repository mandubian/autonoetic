//! End-to-end integration tests for the federation FullJury promotion gate.
//!
//! Tests the complete federation lifecycle:
//! 1. Positive path: legacy gate passes + federation roles + approved escalation → promote succeeds
//! 2. Negative path: federation roles exist but no approved escalation → promote blocked
//! 3. Bypass attempt: legacy gate passes but FullJury blocks without escalation
//! 4. Distinct identity (P-2.17): federation role shares identity with proposer → blocked
//! 5. Legacy regression: no federation roles → legacy gate still works unchanged

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
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, RoleVerdictSummary};
use autonoetic_types::promotion::PromotionRole;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;

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
            description: "Sealed Evaluator".to_string(),
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

fn record_federation_role(
    gateway_dir: &Path,
    artifact_id: &str,
    role: PromotionRole,
    agent_id: &str,
    pass: bool,
) {
    let promo_store = PromotionStore::new(gateway_dir).unwrap();
    let summary = format!("{:?} check — pass={}", role, pass);
    promo_store
        .record_promotion(
            artifact_id.to_string(),
            None,
            None,
            role,
            agent_id,
            pass,
            vec![],
            Some(summary),
        )
        .expect("federation role record should succeed");
}

fn create_approved_escalation(
    store: &Arc<GatewayStore>,
    artifact_id: &str,
    revision_id: &str,
    agent_id: &str,
    role_verdicts: Vec<RoleVerdictSummary>,
) -> String {
    let escalation_id = format!("esc_{:x}", uuid::Uuid::new_v4().as_u128());
    let mut escalation = EscalationMessage::new(
        escalation_id.clone(),
        artifact_id.to_string(),
        agent_id.to_string(),
        revision_id.to_string(),
        role_verdicts,
        "Planner synthesis: all federation roles passed".to_string(),
        "test-root-session".to_string(),
    );
    store.create_escalation(&mut escalation).unwrap();
    store
        .resolve_escalation(
            &escalation_id,
            EscalationStatus::Approved,
            "test-operator",
            Some("approved for federation e2e test"),
        )
        .unwrap();
    escalation_id
}

fn make_verdict(role: PromotionRole, agent_id: &str, passed: bool) -> RoleVerdictSummary {
    let summary = format!("{:?} findings summary", role);
    RoleVerdictSummary {
        role,
        agent_id: agent_id.to_string(),
        passed,
        findings_summary: summary,
        evidence_ref: None,
        recorded_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn setup_test_with_manual_revision(
    agent_id: &str,
    skill_md: &str,
    created_by_id: &str,
) -> (TestSetup, String, String, Arc<GatewayStore>) {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");

    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";
    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), skill_md, main_py);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let revision_id = format!("rev_sha256:fed_e2e_{}", uuid::Uuid::new_v4().as_simple());
    let revision_dir = gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(&revision_id);
    std::fs::create_dir_all(&revision_dir).unwrap();
    std::fs::write(revision_dir.join("SKILL.md"), skill_md).unwrap();

    let rev = AgentRevisionRecord {
        revision_id: revision_id.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.clone()),
        content_digest: format!("sha256:fed-e2e-{}", &revision_id[..20]),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: created_by_id.to_string(),
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

#[test]
fn test_federation_positive_path_promote_succeeds_with_approved_escalation() {
    let agent_id = "fed.positive";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, "specialized_builder.default");

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
        "static_evaluator",
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

    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    );
    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::UnitTestRunner,
        "unit_test_runner.default",
        true,
    );

    let verdicts = vec![
        make_verdict(PromotionRole::StaticEvaluator, "static_evaluator.default", true),
        make_verdict(PromotionRole::UnitTestRunner, "unit_test_runner.default", true),
    ];
    create_approved_escalation(&store, &artifact_id, &revision_id, agent_id, verdicts);

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
        "promote should succeed with approved escalation: {:?}",
        result.err()
    );
    let json = result.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted")
    );
}

#[test]
fn test_federation_blocks_without_approved_escalation() {
    let agent_id = "fed.no.escalation";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, "specialized_builder.default");

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
        "static_evaluator",
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

    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
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
        "promote should fail without approved escalation"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("FullJury") && err.contains("no approved operator escalation"),
        "error should mention FullJury and missing escalation: {err}"
    );
}

#[test]
fn test_federation_blocks_with_pending_escalation() {
    let agent_id = "fed.pending.escalation";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, "specialized_builder.default");

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
        "static_evaluator",
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

    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    );

    let escalation_id = format!("esc_{:x}", uuid::Uuid::new_v4().as_u128());
    let mut escalation = EscalationMessage::new(
        escalation_id,
        artifact_id.clone(),
        agent_id.to_string(),
        revision_id.clone(),
        vec![make_verdict(
            PromotionRole::StaticEvaluator,
            "static_evaluator.default",
            true,
        )],
        "Pending escalation - not yet resolved".to_string(),
        "test-root-session".to_string(),
    );
    store.create_escalation(&mut escalation).unwrap();

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
        "promote should fail with only a pending (not approved) escalation"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("FullJury") && err.contains("no approved operator escalation"),
        "error should mention FullJury: {err}"
    );
}

#[test]
fn test_federation_blocks_when_role_shares_proposer_identity() {
    let proposer_id = "specialized_builder.default";
    let agent_id = "fed.r217.proposer";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, proposer_id);

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
        "static_evaluator",
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

    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        proposer_id,
        true,
    );

    let verdicts = vec![make_verdict(
        PromotionRole::StaticEvaluator,
        proposer_id,
        true,
    )];
    create_approved_escalation(&store, &artifact_id, &revision_id, agent_id, verdicts);

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
        "promote should fail when federation role shares proposer identity"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("FullJury") && err.contains("P-2.17"),
        "error should mention P-2.17 distinct identity: {err}"
    );
}

#[test]
fn test_federation_blocks_when_roles_share_identity() {
    let agent_id = "fed.r217.roles";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, "specialized_builder.default");

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
        "static_evaluator",
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

    let shared_role_id = "dup_agent.default";
    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        shared_role_id,
        true,
    );
    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::UnitTestRunner,
        shared_role_id,
        true,
    );

    let verdicts = vec![
        make_verdict(PromotionRole::StaticEvaluator, shared_role_id, true),
        make_verdict(PromotionRole::UnitTestRunner, shared_role_id, true),
    ];
    create_approved_escalation(&store, &artifact_id, &revision_id, agent_id, verdicts);

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
        "promote should fail when federation roles share identity"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("FullJury")
            && err.contains("P-2.17")
            && err.contains("same agent"),
        "error should mention same-agent P-2.17 violation: {err}"
    );
}

#[test]
fn test_federation_legacy_regression_promote_without_federation_roles() {
    let agent_id = "fed.legacy.regression";
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

    assert!(
        result.is_ok(),
        "legacy path should still work without federation roles: {:?}",
        result.err()
    );
    let json = result.unwrap();
    assert_eq!(json.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted")
    );
}

#[test]
fn test_federation_escalation_rejected_does_not_allow_promotion() {
    let agent_id = "fed.rejected.escalation";
    let skill = high_risk_skill_md(agent_id);
    let (s, artifact_id, revision_id, store) =
        setup_test_with_manual_revision(agent_id, &skill, "specialized_builder.default");

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
        "static_evaluator",
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

    record_federation_role(
        &s.gateway_dir,
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    );

    let escalation_id = format!("esc_{:x}", uuid::Uuid::new_v4().as_u128());
    let verdicts = vec![make_verdict(
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    )];
    let mut escalation = EscalationMessage::new(
        escalation_id.clone(),
        artifact_id.clone(),
        agent_id.to_string(),
        revision_id.clone(),
        verdicts,
        "Operator review requested".to_string(),
        "test-root-session".to_string(),
    );
    store.create_escalation(&mut escalation).unwrap();
    store
        .resolve_escalation(
            &escalation_id,
            EscalationStatus::Rejected,
            "test-operator",
            Some("rejected — insufficient evidence"),
        )
        .unwrap();

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
        "promote should fail with rejected escalation"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("FullJury") && err.contains("no approved operator escalation"),
        "error should mention FullJury: {err}"
    );
}
