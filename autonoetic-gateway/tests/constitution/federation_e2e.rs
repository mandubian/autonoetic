//! End-to-end integration tests for the federation FullJury promotion gate.
//!
//! Tests the complete federation lifecycle:
//! 1. Positive path: legacy gate passes + federation roles + approved escalation → promote succeeds
//! 2. Negative path: federation roles exist but no approved escalation → promote blocked
//! 3. Bypass attempt: legacy gate passes but FullJury blocks without escalation
//! 4. Distinct identity (P-2.17): federation role shares identity with proposer → blocked
//! 5. Legacy regression: no federation roles → legacy gate still works unchanged


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::{approve_request_with_options, ApproveOptions};
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::promotion::PromotionRole;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, RoleVerdictSummary};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

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
        agent: AgentIdentity {
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "Builder".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
        ..TestManifest::new().build()
    }
}

fn evaluator_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "sealed_evaluator.default".to_string(),
            name: "sealed_evaluator.default".to_string(),
            description: "Sealed Evaluator".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: vec!["sandbox.".to_string(), "content.".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn auditor_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "auditor.default".to_string(),
            name: "auditor.default".to_string(),
            description: "Auditor".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: vec!["content.".to_string()],
        }],
        ..TestManifest::new().build()
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
    gw_store: &Arc<GatewayStore>,
    artifact_id: &str,
    role: &str,
    pass: bool,
    session_id: &str,
) {
    let args = crate::support::promotion_trace::build_promotion_record_args(
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

fn seed_smoke_test_task(
    config: &GatewayConfig,
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
) -> (String, String) {
    crate::support::promotion_trace::seed_smoke_test_task(config, store, agent_id, revision_id)
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
    let (smoke_wf, smoke_task) =
        seed_smoke_test_task(config, store.as_ref(), agent_id, revision_id);
    let promote_args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "integration test",
        "smoke_test_workflow_id": smoke_wf,
        "smoke_test_task_id": smoke_task,
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

/// Re-present a structured P-5.11 gate *block* (`Ok(ok:false)`) as `Err(message)`
/// so the `unwrap_err` FullJury failure assertions below read naturally.
fn as_outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) if v["ok"] == serde_json::Value::Bool(false) => {
            Err(v["message"].as_str().unwrap_or_default().to_string())
        }
        other => other,
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
        // Federation tests exercise the escalation/jury/identity gates; isolate
        // them from the new-agent first-admission human gate (its own tests).
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

fn record_federation_role(
    gateway_dir: &Path,
    gw_store: &GatewayStore,
    artifact_id: &str,
    role: PromotionRole,
    agent_id: &str,
    pass: bool,
) {
    let promo_store = PromotionStore::new(gateway_dir).unwrap();
    crate::support::promotion_trace::seed_promotion_store_execution_role(
        &promo_store,
        gw_store,
        artifact_id,
        role,
        agent_id,
        pass,
        &format!("session-{agent_id}"),
        None,
    );
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
        // Freshly-run verdict, not a carry-forward claim. Spelled out rather
        // than defaulted: a carry is a security-relevant assertion, so every
        // construction site should have to say which it is.
        carried_from: None,
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
        // Federation tests exercise the escalation/jury/identity gates; isolate
        // them from the new-agent first-admission human gate (its own tests).
        require_operator_approval_for_new_agents: false,
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
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    );
    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        as_outcome(result.clone()).is_err(),
        "promote should fail without approved escalation"
    );
    let err = as_outcome(result).unwrap_err();
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        as_outcome(result.clone()).is_err(),
        "promote should fail with only a pending (not approved) escalation"
    );
    let err = as_outcome(result).unwrap_err();
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        as_outcome(result.clone()).is_err(),
        "promote should fail when federation role shares proposer identity"
    );
    let err = as_outcome(result).unwrap_err();
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    let shared_role_id = "dup_agent.default";
    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
        &artifact_id,
        PromotionRole::StaticEvaluator,
        shared_role_id,
        true,
    );
    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        as_outcome(result.clone()).is_err(),
        "promote should fail when federation roles share identity"
    );
    let err = as_outcome(result).unwrap_err();
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
        &store,
        &artifact_id,
        "sealed_evaluator",
        true,
        "session-sealed_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
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
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
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
        as_outcome(result.clone()).is_err(),
        "promote should fail with rejected escalation"
    );
    let err = as_outcome(result).unwrap_err();
    assert!(
        err.contains("FullJury") && err.contains("no approved operator escalation"),
        "error should mention FullJury: {err}"
    );
}

// ===========================================================================
// #738 — one operator decision per promotion (merged RevisionPromote)
// ====================================================================================

use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, RevisionPromoteFederationContext,
    ScheduledAction,
};
use autonoetic_types::agent_revision::AgentAliasRecord;

/// Build a merged `RevisionPromote` approval (carrying `federation_context`)
/// linked to an escalation projection, simulating what `federation_escalate`
/// mints under #738 when a capability delta exists. Returns the approval id.
fn create_merged_federation_approval(
    store: &Arc<GatewayStore>,
    artifact_id: &str,
    revision_id: &str,
    agent_id: &str,
    outgoing_revision_id: &str,
    added_capabilities: Vec<String>,
    content_digest: Option<&str>,
    verdicts: Vec<RoleVerdictSummary>,
) -> String {
    let approval_id = format!("apr-merged-{}", uuid::Uuid::new_v4().as_simple());
    let escalation_id = format!("esc_{:x}", uuid::Uuid::new_v4().as_u128());

    // The escalation projection — links back to the approval via
    // approval_request_id (set after create_approval, before create_escalation).
    let role_verdicts_summary = verdicts
        .iter()
        .map(|v| format!("{:?}: {}", v.role, if v.passed { "pass" } else { "fail" }))
        .collect::<Vec<_>>()
        .join(", ");
    let federation_context = RevisionPromoteFederationContext {
        artifact_id: artifact_id.to_string(),
        content_digest: content_digest.map(String::from),
        role_verdicts_summary,
        planner_synthesis: "All federation roles passed; capability delta acknowledged."
            .to_string(),
    };

    let action = ScheduledAction::RevisionPromote {
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        outgoing_revision_id: outgoing_revision_id.to_string(),
        added_capabilities: added_capabilities.clone(),
        broadened_capabilities: vec![],
        payload: Some(serde_json::json!({
            "escalation_id": escalation_id,
            "artifact_id": artifact_id,
            "revision_id": revision_id,
            "federation_review": true,
        })),
        federation_context: Some(federation_context),
    };

    let mut approval = ApprovalRequest {
        request_id: approval_id.clone(),
        agent_id: agent_id.to_string(),
        session_id: "session-merged-promote".to_string(),
        action,
        created_at: (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("test-root-session".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut approval).unwrap();

    // Resolve the approval as Approved with the required capability
    // acknowledgements + confirm phrase (Critical hardening). The dwell
    // requirement is bypassed by setting approval_dwell_multiplier = 0 in the
    // test config (setup_test_with_manual_revision uses Default which is 1.0,
    // so instead we backdate created_at far enough — 60s satisfies the 5s
    // Critical dwell). The confirm phrase is `promote <agent> <rev[..16]>`.
    let rev_prefix = &revision_id[..revision_id.len().min(16)];
    let confirm = format!("promote {} {}", agent_id, rev_prefix);
    approve_request_with_options(
        &GatewayConfig::default(),
        Some(store.as_ref()),
        &approval_id,
        "test-operator",
        None,
        None,
        None,
        None,
        ApproveOptions {
            acknowledged_capabilities: added_capabilities.clone(),
            confirm_phrase: Some(confirm),
            ..Default::default()
        },
    )
    .unwrap();

    // Escalation projection linked to the merged approval.
    let mut escalation = EscalationMessage::new(
        escalation_id.clone(),
        artifact_id.to_string(),
        agent_id.to_string(),
        revision_id.to_string(),
        verdicts,
        "Merged promotion review".to_string(),
        "test-root-session".to_string(),
    );
    escalation.artifact_digest = content_digest.map(String::from);
    escalation.approval_request_id = Some(approval_id.clone());
    store.create_escalation(&mut escalation).unwrap();

    approval_id
}

/// Set up an existing agent (alias pointing at an outgoing revision with no
/// caps) and an incoming revision with broadened caps (NetworkAccess) + an
/// artifact + federation roles. Returns everything needed to exercise the
/// merged-promote path.
fn setup_existing_agent_with_broadened_caps_and_federation(
    agent_id: &str,
) -> (
    TestSetup,
    String,  // artifact_id
    String,  // revision_id (incoming)
    String,  // outgoing_revision_id
    Arc<GatewayStore>,
) {
    let outgoing_skill = format!(
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
  description: "Existing agent with no caps"
execution_mode: script
script_entry: main.py
---
No caps."#
    );
    let incoming_skill = high_risk_skill_md(agent_id); // has NetworkAccess

    let (s, artifact_id, incoming_rev, store) =
        setup_test_with_manual_revision(agent_id, &incoming_skill, "specialized_builder.default");

    // Write the outgoing revision's SKILL.md and insert the revision + alias.
    let outgoing_rev = format!("rev_sha256:outgoing_{}", uuid::Uuid::new_v4().as_simple());
    let outgoing_dir = s
        .gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(&outgoing_rev);
    std::fs::create_dir_all(&outgoing_dir).unwrap();
    std::fs::write(outgoing_dir.join("SKILL.md"), &outgoing_skill).unwrap();

    let outgoing_record = AgentRevisionRecord {
        revision_id: outgoing_rev.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.clone()),
        content_digest: format!("sha256:outgoing-{}", &outgoing_rev[..20]),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "specialized_builder.default".to_string(),
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
    store.insert_agent_revision(&outgoing_record).unwrap();

    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: outgoing_rev.clone(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias).unwrap();

    // Legacy evaluator + auditor records (the Full gate still requires these
    // independent of the federation FullJury gate).
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    // Federation roles on the artifact.
    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
        &artifact_id,
        PromotionRole::StaticEvaluator,
        "static_evaluator.default",
        true,
    );
    record_federation_role(
        &s.gateway_dir,
        store.as_ref(),
        &artifact_id,
        PromotionRole::UnitTestRunner,
        "unit_test_runner.default",
        true,
    );

    (s, artifact_id, incoming_rev, outgoing_rev, store)
}

#[test]
fn merged_revision_promote_covers_both_gates_for_existing_agent() {
    // #738 core scenario: an existing agent with broadened caps AND federation
    // jury verdicts. ONE merged RevisionPromote approval (carrying federation
    // context) must satisfy BOTH the capability-delta gate and the FullJury
    // review gate — no second approval required.
    let agent_id = "fed.merged.existing";
    let (s, artifact_id, revision_id, outgoing_rev, store) =
        setup_existing_agent_with_broadened_caps_and_federation(agent_id);

    let verdicts = vec![
        make_verdict(PromotionRole::StaticEvaluator, "static_evaluator.default", true),
        make_verdict(PromotionRole::UnitTestRunner, "unit_test_runner.default", true),
    ];
    let _approval_id = create_merged_federation_approval(
        &store,
        &artifact_id,
        &revision_id,
        agent_id,
        &outgoing_rev,
        vec!["NetworkAccess".to_string()],
        None, // no digest binding for this test
        verdicts,
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
        "promote should succeed with one merged approval: {:?}",
        result.err()
    );
    let json = result.unwrap();
    assert_eq!(
        json.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "merged approval should cover both gates: {json}"
    );
}

#[test]
fn merged_revision_promote_blocked_without_approval() {
    // Same setup, but NO merged approval — both gates must block.
    let agent_id = "fed.merged.blocked";
    let (s, artifact_id, revision_id, outgoing_rev, store) =
        setup_existing_agent_with_broadened_caps_and_federation(agent_id);
    let _ = (artifact_id, outgoing_rev);

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

    let outcome = as_outcome(result);
    assert!(
        outcome.is_err(),
        "promote should be blocked without the merged approval"
    );
}

#[test]
fn merged_approval_digest_mismatch_does_not_satisfy_jury() {
    // #653 structural fix: an approval whose content_digest differs from the
    // escalation projection's artifact_digest must NOT satisfy the FullJury gate.
    let agent_id = "fed.merged.digest";
    let (s, artifact_id, revision_id, outgoing_rev, store) =
        setup_existing_agent_with_broadened_caps_and_federation(agent_id);

    let verdicts = vec![
        make_verdict(PromotionRole::StaticEvaluator, "static_evaluator.default", true),
        make_verdict(PromotionRole::UnitTestRunner, "unit_test_runner.default", true),
    ];

    // Create the merged approval with content_digest = A, but the escalation
    // projection with artifact_digest = B. The FullJury digest binding must
    // reject the mismatch and fall through to the legacy lookup (which also
    // finds no match because the projection is the merged one, not a legacy
    // SessionEscalate approval).
    let approval_id = format!("apr-merged-digest-{}", uuid::Uuid::new_v4().as_simple());
    let escalation_id = format!("esc_{:x}", uuid::Uuid::new_v4().as_u128());
    let federation_context = RevisionPromoteFederationContext {
        artifact_id: artifact_id.clone(),
        content_digest: Some("sha256:approval-digest-A".to_string()),
        role_verdicts_summary: "verdicts".to_string(),
        planner_synthesis: "synthesis".to_string(),
    };
    let action = ScheduledAction::RevisionPromote {
        agent_id: agent_id.to_string(),
        revision_id: revision_id.clone(),
        outgoing_revision_id: outgoing_rev.clone(),
        added_capabilities: vec!["NetworkAccess".to_string()],
        broadened_capabilities: vec![],
        payload: Some(serde_json::json!({
            "escalation_id": escalation_id,
            "artifact_id": artifact_id,
            "revision_id": revision_id,
            "federation_review": true,
        })),
        federation_context: Some(federation_context),
    };
    let mut approval = ApprovalRequest {
        request_id: approval_id.clone(),
        agent_id: agent_id.to_string(),
        session_id: "session-digest-test".to_string(),
        action,
        created_at: (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339(),
        reason: None,
        evidence_ref: None,
        root_session_id: Some("test-root-session".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut approval).unwrap();
    // Approve through the real decision path so the R++2 invariants (Critical
    // dwell via backdated created_at, confirm phrase, capability
    // acknowledgements) are exercised rather than bypassed (#746 review).
    let rev_prefix = &revision_id[..revision_id.len().min(16)];
    approve_request_with_options(
        &GatewayConfig::default(),
        Some(store.as_ref()),
        &approval_id,
        "test-operator",
        None,
        None,
        None,
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            confirm_phrase: Some(format!("promote {} {}", agent_id, rev_prefix)),
            ..Default::default()
        },
    )
    .unwrap();

    // Escalation projection with a DIFFERENT digest.
    let mut escalation = EscalationMessage::new(
        escalation_id.clone(),
        artifact_id.clone(),
        agent_id.to_string(),
        revision_id.clone(),
        verdicts,
        "Merged review with mismatched digest".to_string(),
        "test-root-session".to_string(),
    );
    escalation.artifact_digest = Some("sha256:projection-digest-B".to_string());
    escalation.approval_request_id = Some(approval_id.clone());
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

    // The digest mismatch invalidates the merged approval for the FullJury gate.
    // The capability gate also doesn't bypass (the approval's outgoing_revision_id
    // matches, but the FullJury gate still blocks → promote fails).
    let outcome = as_outcome(result);
    assert!(
        outcome.is_err(),
        "promote should be blocked when the merged approval's digest does not \
         match the escalation projection's digest (#653): {:?}",
        outcome.ok()
    );
}

#[test]
fn merged_approval_baseline_drift_regates() {
    // #746 review (TOCTOU): a merged approval acknowledged against outgoing
    // revision A must NOT satisfy the gates after the alias moved to B — the
    // promote must re-gate for a fresh operator decision, exactly like
    // check_revision_promote_approval's baseline rule.
    let agent_id = "fed.merged.drift";
    let (s, artifact_id, revision_id, outgoing_rev, store) =
        setup_existing_agent_with_broadened_caps_and_federation(agent_id);

    let verdicts = vec![
        make_verdict(PromotionRole::StaticEvaluator, "static_evaluator.default", true),
        make_verdict(PromotionRole::UnitTestRunner, "unit_test_runner.default", true),
    ];
    // A valid merged approval (approved via the real decision path, acked caps,
    // confirm phrase) against the CURRENT baseline.
    let _approval_id = create_merged_federation_approval(
        &store,
        &artifact_id,
        &revision_id,
        agent_id,
        &outgoing_rev,
        vec!["NetworkAccess".to_string()],
        None,
        verdicts,
    );

    // Baseline drift: another promotion lands in between — the alias now points
    // to a different (zero-cap) revision. Clone the outgoing revision's on-disk
    // SKILL.md + record under a new id so delta computation still works.
    let drifted_rev = format!("rev_sha256:drifted_{}", uuid::Uuid::new_v4().as_simple());
    let outgoing_dir = s
        .gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(&outgoing_rev);
    let drifted_dir = s
        .gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(&drifted_rev);
    std::fs::create_dir_all(&drifted_dir).unwrap();
    std::fs::copy(outgoing_dir.join("SKILL.md"), drifted_dir.join("SKILL.md")).unwrap();
    let drifted_record = AgentRevisionRecord {
        revision_id: drifted_rev.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.clone()),
        content_digest: format!("sha256:drifted-{}", &drifted_rev[..20]),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "specialized_builder.default".to_string(),
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
    store.insert_agent_revision(&drifted_record).unwrap();
    let drifted_alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: drifted_rev.clone(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test-drift".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&drifted_alias).unwrap();

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

    let outcome = as_outcome(result);
    assert!(
        outcome.is_err(),
        "a merged approval acknowledged against a moved baseline must not \
         satisfy the gates — the promote must re-gate (#746 TOCTOU): {:?}",
        outcome.ok()
    );
}

// ---------------------------------------------------------------------------
// federation_escalate revision resolution (escalate-before-install)
// ---------------------------------------------------------------------------

/// Setup like `setup_test` but WITHOUT seeding a revision — the new-agent
/// escalate-before-install state (artifact built, agent-factory not run yet).
fn setup_test_unseeded(skill_md: &str) -> (TestSetup, String, Arc<GatewayStore>) {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let builder_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&builder_dir).expect("builder dir should create");

    let main_py = "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'status': 'ok'}))\n";
    let (artifact_id, gateway_dir) = build_agent_bundle(temp.path(), skill_md, main_py);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        // Keep the new-agent gate ON so the capability delta is computed and
        // load_artifact_capabilities (the unseeded fallback) is exercised.
        require_operator_approval_for_new_agents: true,
        ..Default::default()
    };

    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
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
    (setup, artifact_id, store)
}

fn escalate(
    s: &TestSetup,
    store: &Arc<GatewayStore>,
    args: serde_json::Value,
) -> serde_json::Value {
    let raw = s
        .registry
        .execute(
            "federation_escalate",
            &s.b_manifest,
            &s.b_policy,
            &s.builder_dir,
            Some(&s.gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-escalate"),
            None,
            Some(&s.config),
            Some(store.clone()),
            None,
        )
        .expect("federation_escalate should not hard-error");
    serde_json::from_str(&raw).unwrap()
}

fn passing_verdicts() -> serde_json::Value {
    serde_json::json!([
        {
            "role": "auditor",
            "agent_id": "auditor.default",
            "passed": true,
            "findings_summary": "no findings",
            "recorded_at": chrono::Utc::now().to_rfc3339(),
        }
    ])
}

/// A NEW agent has no seeded revision at escalate time. Omitting revision_id
/// must bind the review to the artifact: capabilities come from the artifact's
/// SKILL.md and the escalation is recorded under the derived unseeded key —
/// not a caller-invented placeholder (the session-2e758277 'rev-initial' bug).
#[test]
fn escalate_unseeded_new_agent_binds_to_artifact() {
    let agent_id = "unseeded-new-agent";
    let (s, artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));

    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed; recommend promotion.",
            "root_session_id": "root-unseeded",
        }),
    );

    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "unseeded new-agent escalation should succeed: {parsed}"
    );
    assert_eq!(
        parsed.get("status").and_then(|v| v.as_str()),
        Some("pending"),
        "should create a pending operator review: {parsed}"
    );

    // The escalation projection is keyed by the derived unseeded revision id,
    // so the promote-side artifact-scoped lookup can bind it later.
    let derived = format!("unseeded:{}", artifact_id);
    let esc = store
        .find_escalation(&artifact_id, &derived, EscalationStatus::Pending)
        .unwrap();
    assert!(
        esc.is_some(),
        "escalation should be recorded under the derived unseeded key"
    );
}

/// An invented / unknown revision id must be refused with an explicit error,
/// not deferred to a downstream SKILL.md read failure.
#[test]
fn escalate_unknown_revision_id_is_refused_explicitly() {
    let agent_id = "unseeded-typo-agent";
    let (s, artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));

    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "revision_id": "rev-initial",
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-typo",
        }),
    );

    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "an invented revision id must be refused: {parsed}"
    );
    let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("does not exist") && msg.contains("OMIT revision_id"),
        "error must name the phantom revision and the omit-for-new-agent fix: {msg}"
    );
}

/// An EXISTING agent (alias resolves) must escalate a real seeded revision —
/// omitting revision_id is a caller error there, not the artifact-bound path.
#[test]
fn escalate_existing_agent_requires_seeded_revision() {
    let agent_id = "existing-agent-escalate";
    let (s, artifact_id, revision_id, store) =
        setup_test(agent_id, &high_risk_skill_md(agent_id));

    let alias = autonoetic_types::agent_revision::AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.clone(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias).unwrap();

    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-existing",
        }),
    );

    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "existing agent without revision_id must be refused: {parsed}"
    );
    let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("already installed"),
        "error must say the agent is installed and a seeded revision is required: {msg}"
    );

    // The short revision id must also resolve (short_id_index), not just the
    // full rev_sha256 form. Two accepted forms, both regression-checked:
    //   (a) the BARE short token as stored in short_id_index
    //   (b) the `rev_`-prefixed form presented to LLMs as `agent@rev_<short>`
    //       (the form the planner is shown and will pass back). Without the
    //       prefix-strip in federation_escalate this second form was rejected
    //       as an unknown revision id.
    let short = store
        .list_short_ids_for_agent(agent_id)
        .unwrap()
        .into_iter()
        .find(|(_, full)| full == &revision_id)
        .map(|(short, _)| short)
        .expect("seeded revision should have a short id");

    // (a) bare short token
    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "revision_id": short,
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-existing-short",
        }),
    );
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "bare short revision id should resolve via short_id_index: {parsed}"
    );
    // Clear the pending escalation so the (artifact, revision) dedup key is
    // free for the prefixed-form retry below.
    let esc_id = parsed
        .get("escalation_id")
        .and_then(|v| v.as_str())
        .expect("escalate returns an escalation_id");
    store
        .resolve_escalation(esc_id, EscalationStatus::Rejected, "test", None)
        .unwrap();

    // (b) rev_-prefixed short id (the form shown in `agent@rev_<short>`)
    let prefixed = format!("rev_{}", short);
    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "revision_id": prefixed,
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-existing-short-prefixed",
        }),
    );
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "rev_-prefixed short id (the form shown to LLMs) must resolve: {parsed}"
    );
}

/// End-to-end escalate-before-install lifecycle:
///
/// 1. `federation_escalate` runs against a NEW agent with no seeded revision
///    (revision_id omitted) — the escalation is recorded under the derived
///    `unseeded:<artifact>` key and the merged `RevisionPromote` approval is
///    created in Pending.
/// 2. The operator approves that merged approval (capability ack + confirm
///    phrase) — both the R++2 capability gate AND the FullJury jury gate are
///    satisfied by this single operator decision.
/// 3. agent-factory seeds the real revision (`rev_sha256:...`) into the store
///    and onto disk.
/// 4. `agent_revision_promote` runs against the real revision id. The
///    `unseeded:`-keyed escalation must still satisfy the gates via the
///    artifact-scoped fallbacks (`find_approved_escalation_for_artifact` in
///    both the new-agent capability gate and the legacy FullJury fallback) so
///    the promote succeeds without a second operator approval.
///
/// This is the transition the unit escalate-side tests do not cover, and the
/// one place where an `unseeded:` → `rev_sha256:` key mismatch could
/// silently break the binding.
#[test]
fn escalate_unseeded_approval_honored_after_revision_seeded() {
    let agent_id = "unseeded-lifecycle-agent";
    let (mut s, artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));

    // Zero the dwell multiplier so the operator approval can complete in the
    // same test tick — the merged approval created by federation_escalate would
    // otherwise require a 5s Critical dwell before the confirm phrase is
    // accepted. The new-agent gate stays ON (set by setup_test_unseeded) so the
    // capability delta is still computed and load_artifact_capabilities is
    // exercised.
    s.config.approval_dwell_multiplier = 0.0;

    // (1) Escalate before install: omit revision_id. The capability delta is
    // computed from the artifact's SKILL.md (NetworkAccess) and a merged
    // RevisionPromote approval is created in Pending.
    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": artifact_id,
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed; recommend promotion.",
            "root_session_id": "root-unseeded-lifecycle",
        }),
    );
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "unseeded escalation should succeed: {parsed}"
    );
    assert_eq!(
        parsed.get("status").and_then(|v| v.as_str()),
        Some("pending"),
        "escalation should start pending: {parsed}"
    );
    let approval_id = parsed
        .get("approval_request_id")
        .and_then(|v| v.as_str())
        .expect("pending escalation returns an approval_request_id")
        .to_string();

    // (2) Operator approves the merged approval with the capability ack and
    // confirm phrase. The confirm phrase format is `promote <agent> <rev[..16]>`;
    // here the recorded revision_id is the derived `unseeded:` key.
    let derived_revision_id = format!("unseeded:{}", artifact_id);
    let rev_prefix = &derived_revision_id[..derived_revision_id.len().min(16)];
    let confirm = format!("promote {} {}", agent_id, rev_prefix);
    approve_request_with_options(
        &s.config,
        Some(store.as_ref()),
        &approval_id,
        "test-operator",
        None,
        None,
        None,
        None,
        ApproveOptions {
            acknowledged_capabilities: vec!["NetworkAccess".to_string()],
            confirm_phrase: Some(confirm),
            ..Default::default()
        },
    )
    .expect("operator approval should succeed");

    // The merged approval is now Approved; the escalation projection remains
    // keyed under `unseeded:<artifact>` (the federation_escalate path recorded
    // it before the real revision existed). The promote-side artifact-scoped
    // lookups must find it.
    let approved_unseeded = store
        .find_escalation(
            &artifact_id,
            &derived_revision_id,
            EscalationStatus::Approved,
        )
        .unwrap();
    assert!(
        approved_unseeded.is_some(),
        "unseeded escalation should be approved after operator decision"
    );

    // (3) agent-factory seeds the real revision: insert a Candidate record and
    // write the SKILL.md into the revisions/agents/<id>/<rev> directory so the
    // promote path can load capabilities from the revision dir.
    let real_revision_id =
        format!("rev_sha256:lifecycle_{}", uuid::Uuid::new_v4().as_simple());
    let revision_dir = s
        .gateway_dir
        .join("revisions/agents")
        .join(agent_id)
        .join(&real_revision_id);
    std::fs::create_dir_all(&revision_dir).unwrap();
    std::fs::write(
        revision_dir.join("SKILL.md"),
        high_risk_skill_md(agent_id),
    )
    .unwrap();
    let rev = AgentRevisionRecord {
        revision_id: real_revision_id.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.clone()),
        content_digest: format!("sha256:lifecycle-{}", &real_revision_id[..20]),
        runtime_lock_hash: "sha256:test_lock".to_string(),
        manifest_hash: "sha256:test_manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "agent-factory.default".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Candidate,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();

    // Record the evaluator + auditor promotion records the legacy gate
    // requires for any code-bearing NetworkAccess agent.
    let eval_manifest = evaluator_manifest();
    let eval_policy = PolicyEngine::new(eval_manifest.clone());
    record_promotion(
        &s.registry,
        &eval_manifest,
        &eval_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        &store,
        &artifact_id,
        "static_evaluator",
        true,
        "session-static_evaluator",
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
        &store,
        &artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    // Sanity: still a new agent (no alias) — the new-agent capability gate
    // path is the one in play, not the existing-agent broadening path.
    assert!(
        store.resolve_alias(agent_id).unwrap().is_none(),
        "fixture: agent should still be new (no alias) at promote time"
    );

    // (4) Promote under the REAL revision id. The merged approval's stored
    // revision_id is the unseeded key, but the gates fall back to
    // artifact-scoped lookups (`find_approved_escalation_for_artifact`) and
    // so the previously-approved escalation must satisfy both gates.
    let result = try_promote(
        &s.registry,
        &s.b_manifest,
        &s.b_policy,
        &s.builder_dir,
        &s.gateway_dir,
        &s.config,
        store.clone(),
        agent_id,
        &real_revision_id,
    );

    let outcome = as_outcome(result.clone());
    assert!(
        outcome.is_ok(),
        "promote should succeed: the approved unseeded escalation must bind to \
         the real revision via the artifact-scoped lookups — got: {:?}",
        outcome.err()
    );
    let json = result.unwrap();
    assert_eq!(
        json.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promote should return ok=true: {json}"
    );
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted"),
        "promote should report status=promoted: {json}"
    );

    // The promote landed: the alias now points at the real revision.
    let alias = store
        .resolve_alias(agent_id)
        .unwrap()
        .expect("alias should exist after a successful promote");
    assert_eq!(
        alias.revision_id, real_revision_id,
        "alias should point at the real revision after promote"
    );
}

/// An unresolved `ar.*` artifact_ref must be refused explicitly. Silently
/// falling back to the literal ref string as artifact_id would bind the
/// escalation under a non-canonical key like `unseeded:ar.deadbeef` and break
/// every promote-side artifact lookup (Copilot #2 on PR #751).
#[test]
fn escalate_unresolvable_artifact_ref_is_refused() {
    let agent_id = "unresolved-ref-agent";
    let (s, _artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));

    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_ref": "ar.deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef_deadbeef",
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-unresolved-ref",
        }),
    );

    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "an unresolvable artifact_ref must be refused, not silently used as \
         the artifact_id: {parsed}"
    );
    let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("could not be resolved"),
        "error must explain the ref did not resolve: {msg}"
    );
}

/// A short id whose short_id_index entry points at a full revision id that no
/// longer exists (stale/orphaned index row) must be a hard validation error.
/// Treating it as the unseeded new-agent path would silently re-bind the
/// approval under `unseeded:<artifact>` for what the caller meant as an
/// existing revision (Copilot #3 on PR #751).
#[test]
fn escalate_orphaned_short_id_is_refused() {
    let agent_id = "orphaned-short-id-agent";
    let (s, _artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));

    // Plant an orphaned short_id_index entry: the short id resolves, but the
    // full revision record it points at was never written (or was deleted).
    let phantom_full_id =
        format!("rev_sha256:phantom_{}", uuid::Uuid::new_v4().as_simple());
    store
        .register_short_id(&phantom_full_id, "phantom00")
        .unwrap();
    // Sanity: the short id resolves but the full revision does not.
    assert_eq!(
        store.lookup_short_id("phantom00").unwrap().as_deref(),
        Some(phantom_full_id.as_str())
    );
    assert!(store
        .get_agent_revision(&phantom_full_id)
        .unwrap()
        .is_none());

    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "revision_id": "rev_phantom00",
            "role_verdicts": passing_verdicts(),
            "planner_synthesis": "All roles passed.",
            "root_session_id": "root-orphaned-short",
        }),
    );

    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "an orphaned short_id_index entry must be refused, not silently \
         re-bound under the unseeded path: {parsed}"
    );
    let msg = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("stale") && msg.contains("short_id_index"),
        "error must explain the short_id_index is stale: {msg}"
    );
}

// ---------------------------------------------------------------------------
// carry-forward lineage ancestry table (#1067 follow-up)
// ---------------------------------------------------------------------------

/// #1067 follow-up: an accepted carry records a `carry_forward_lineage` edge
/// (source artifact, role, strictness, prior digests) in the gateway store,
/// and the ancestry walk answers the chain back to the artifact whose gates
/// ran fresh. A rejected claim leaves no row.
#[test]
fn escalated_carry_records_ancestry_edge_and_rejected_leaves_none() {
    let agent_id = "carry-lineage-agent";
    let (mut s, current_artifact_id, store) = setup_test_unseeded(&high_risk_skill_md(agent_id));
    // Carries are opt-in: default strictness `off` rejects every claim, so
    // flip the dial like an operator would.
    s.config.federation.carry_forward_strictness =
        autonoetic_types::config::CarryForwardStrictness::Conservative;

    // Two PRIOR artifacts with distinct bytes (distinct ids). Their promotion
    // records get the CURRENT bundle's code/contract digests — the verified
    // match that makes a carry sound — or a deliberately different code
    // digest to force rejection.
    let (prior_ok_id, _) = build_agent_bundle(
        s._temp.path(),
        "---\nname: prior-ok\n---\n# prior ok, different bytes\n",
        "#!/usr/bin/env python3\nprint('prior ok')\n",
    );
    let (prior_bad_id, _) = build_agent_bundle(
        s._temp.path(),
        "---\nname: prior-bad\n---\n# prior bad, different bytes\n",
        "#!/usr/bin/env python3\nprint('prior bad')\n",
    );

    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&s.gateway_dir).unwrap();
    let current_bundle = artifact_store.inspect(&current_artifact_id).unwrap();
    let digests = autonoetic_gateway::runtime::federation_carry_forward::compute_federation_digests(
        &current_bundle,
        &artifact_store,
    );
    let cur_code = digests
        .code_digest
        .clone()
        .expect("main.py is code — code_digest must be present");
    let cur_contract = digests
        .contract_digest
        .clone()
        .expect("frontmatter contract fields — contract_digest must be present");

    let promo = PromotionStore::new(&s.gateway_dir).unwrap();
    for (id, code) in [
        (&prior_ok_id, cur_code.clone()),
        (&prior_bad_id, "sha256:different-code".to_string()),
    ] {
        promo
            .record_promotion(
                id.clone(),
                Some("sha256:art".to_string()),
                Some("sha256:content".to_string()),
                PromotionRole::UnitTestRunner,
                "unit_test_runner.default",
                true,
                vec![],
                Some("all clean".to_string()),
                None,
            )
            .unwrap();
        promo
            .set_federation_digests(
                id,
                Some(code),
                Some(cur_contract.clone()),
                Some("sha256:prose".to_string()),
            )
            .unwrap();
    }

    let carried_verdict = |prior_ref: &str| {
        serde_json::json!([{
            "role": "unit_test_runner",
            "agent_id": "unit_test_runner.default",
            "passed": true,
            "findings_summary": "no findings",
            "recorded_at": chrono::Utc::now().to_rfc3339(),
            "carried_from": {
                "prior_artifact_ref": prior_ref,
                "role": "unit_test_runner",
                "justification": "prose-only fix; code and contract unchanged",
            },
        }])
    };

    // Rejected claim first: mismatched code digest must fail the escalate
    // with a structured rejection and record NO lineage row.
    let raw = s
        .registry
        .execute(
            "federation_escalate",
            &s.b_manifest,
            &s.b_policy,
            &s.builder_dir,
            Some(&s.gateway_dir),
            &serde_json::to_string(&serde_json::json!({
                "agent_id": agent_id,
                "artifact_id": current_artifact_id,
                "role_verdicts": carried_verdict(&prior_bad_id),
                "planner_synthesis": "Propose carry.",
                "root_session_id": "root-lineage",
            }))
            .unwrap(),
            Some("session-escalate"),
            None,
            Some(&s.config),
            Some(store.clone()),
            None,
        );
    assert!(
        raw.is_err(),
        "a mismatched carry claim must be rejected: {raw:?}"
    );
    assert!(
        format!("{}", raw.unwrap_err()).contains("carry_forward_rejected"),
        "rejection must carry the structured carry_forward_rejected reason"
    );
    assert!(
        store.get_carry_lineage(&current_artifact_id).unwrap().is_empty(),
        "a rejected claim must not leave a lineage row"
    );

    // Accepted claim: escalate succeeds, and the ancestry edge is recorded.
    let parsed = escalate(
        &s,
        &store,
        serde_json::json!({
            "agent_id": agent_id,
            "artifact_id": current_artifact_id,
            "role_verdicts": carried_verdict(&prior_ok_id),
            "planner_synthesis": "Propose carry.",
            "root_session_id": "root-lineage",
        }),
    );
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "a verified carry must be accepted: {parsed}"
    );

    let rows = store.get_carry_lineage(&current_artifact_id).unwrap();
    assert_eq!(rows.len(), 1, "one accepted carry, one edge");
    assert_eq!(rows[0].role, "unit_test_runner");
    assert_eq!(rows[0].source_artifact_id, prior_ok_id);
    assert_eq!(rows[0].source_artifact_ref, prior_ok_id);
    assert_eq!(rows[0].strictness, "conservative");
    assert_eq!(
        rows[0].source_code_digest.as_deref(),
        Some(cur_code.as_str())
    );
    assert_eq!(
        rows[0].source_contract_digest.as_deref(),
        Some(cur_contract.as_str())
    );

    // The ancestry walk answers the chain: current -> prior_ok (which ran
    // fresh — no edge on it).
    let chain = store.walk_carry_ancestors(&current_artifact_id).unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].source_artifact_id, prior_ok_id);
    assert!(
        store.walk_carry_ancestors(&prior_ok_id).unwrap().is_empty(),
        "the original gate run is the root of the chain"
    );
}
