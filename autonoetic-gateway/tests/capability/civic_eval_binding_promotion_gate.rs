//! Civic-eval binding promotion gate (#774 E.3 phase 2 follow-up).
//!
//! The advisory surface (#772 E.3) always reports the latest civic-core-v1
//! score in the promotion response. When `civic_eval_binding.enabled` is set
//! and the revision is high-risk, the binding gate rejects promotion if the
//! latest completed civic eval run falls below `min_pass_ratio`. Advisory
//! stays the floor (RFC invariant 5): binding is opt-in, default off.
//!
//! These tests verify through the real `agent_revision_promote` path:
//!   - binding disabled (default) → advisory surface present, promotion
//!     proceeds regardless of civic score
//!   - binding enabled + run below threshold → promotion rejected with
//!     `civic_eval_below_threshold`
//!   - binding enabled + run at/above threshold → promotion proceeds, and
//!     the advisory surface reports `binding: "passed"`
//!   - binding enabled + no run → pass-through (missing-run policy)
//!
//! The threshold math itself is unit-tested in `runtime::civic_evals`.


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{CivicEvalBindingConfig, GatewayConfig};
use autonoetic_types::evaluation::{EvalRunRecord, EvalRunStatus};
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::civic_evals::CIVIC_EVAL_SUITE_ID;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

const AGENT_ID: &str = "civic.binding.test";

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
  description: "High-risk revision for civic-binding test."
capabilities:
  - type: ReadAccess
    scopes: ["self.*"]
  - type: AgentSpawn
    max_children: 2
llm_config:
  provider: "openai"
  model: "test-model"
execution_mode: reasoning
---
# {agent_id}
"#,
        agent_id = agent_id
    )
}

fn build_intent_only_artifact(base_dir: &Path, agent_id: &str, skill_md: &str) -> (String, PathBuf) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "build-session";
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
    let skill_name = format!("{}.skill.md", agent_id);
    for (path, content) in [
        (skill_name.as_str(), skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, path, &handle)
            .unwrap();
    }
    let bundle = artifact_store
        .build_with_kind(
            &[skill_name.clone(), "runtime.lock".to_string()],
            None,
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();
    (bundle.artifact_id, gateway_dir)
}

fn proposer_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "specialized_builder.default".to_string(),
            name: "specialized_builder.default".to_string(),
            description: "Proposer".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::AgentRevision {
            patterns: vec!["*".to_string()],
        }],
        ..TestManifest::new().build()
    }
}

fn gate_manifest(id: &str, allowed: Vec<&str>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: id.to_string(),
            name: id.to_string(),
            description: "Gate role".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: allowed.into_iter().map(String::from).collect(),
        }],
        ..TestManifest::new().build()
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    gateway_dir: PathBuf,
    proposer_dir: PathBuf,
    config: GatewayConfig,
    registry: autonoetic_gateway::runtime::tools::NativeToolRegistry,
    proposer: AgentManifest,
    proposer_policy: PolicyEngine,
    store: Arc<GatewayStore>,
    artifact_id: String,
    revision_id: String,
}

fn setup() -> Fixture {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let proposer_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&proposer_dir).expect("proposer dir");

    let (artifact_id, gateway_dir) =
        build_intent_only_artifact(temp.path(), AGENT_ID, &high_risk_skill_md(AGENT_ID));

    let config = GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir: agents_dir.clone(),
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let proposer = proposer_manifest();
    let proposer_policy = PolicyEngine::new(proposer.clone());

    // Seed the civic suite so eval-run lookups are realistic.
    autonoetic_gateway::runtime::civic_evals::ensure_civic_eval_suite(&store, "test-node")
        .expect("seed civic suite");

    let args = serde_json::json!({
        "agent_id": AGENT_ID,
        "artifact_id": artifact_id,
        "instructions": format!("# {}\n\nHigh-risk test agent.", AGENT_ID),
        "description": "High-risk test agent.",
        "capabilities": [
            {"type": "ReadAccess", "scopes": ["self.*"]},
            {"type": "AgentSpawn", "max_children": 2},
        ],
        "execution_mode": "reasoning",
        "llm_config": {"provider": "openai", "model": "test-model"},
    });
    let result = registry
        .execute(
            "agent_revision_create_from_intent",
            &proposer,
            &proposer_policy,
            &proposer_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-create"),
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .expect("create_from_intent should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["ok"], true,
        "create_from_intent failed: {parsed}"
    );
    let revision_id = parsed["revision_id"].as_str().unwrap().to_string();

    Fixture {
        _temp: temp,
        gateway_dir,
        proposer_dir,
        config,
        registry,
        proposer,
        proposer_policy,
        store,
        artifact_id,
        revision_id,
    }
}

/// Record both gate passes (Full mode: auditor + evaluator, distinct
/// identities) so the high-risk revision clears the artifact gate and reaches
/// the civic-binding gate. The `promotion_record` tool is gated by
/// `is_promotion_agent`, so the agent ids must be exactly the allowed ones
/// (`auditor.default`, `sealed_evaluator.default`) — the role string selects
/// which verdict slot the record fills.
fn record_full_gate_pass(f: &Fixture) {
    // (agent_id, role_string)
    for (id, role) in [
        ("auditor.default", "auditor"),
        ("sealed_evaluator.default", "evaluator"),
    ] {
        let manifest = gate_manifest(id, vec!["content."]);
        let policy = PolicyEngine::new(manifest.clone());
        let args = crate::support::promotion_trace::build_promotion_record_args(
            f.store.as_ref(),
            &f.artifact_id,
            role,
            true,
            &format!("session-{role}"),
        );
        let result = f
            .registry
            .execute(
                "promotion_record",
                &manifest,
                &policy,
                &f.proposer_dir,
                Some(&f.gateway_dir),
                &serde_json::to_string(&args).unwrap(),
                Some(&format!("session-{role}")),
                None,
                Some(&f.config),
                Some(f.store.clone()),
                None,
            )
            .expect("promotion_record should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true, "{role} promotion_record failed: {parsed}");
    }
}

fn seed_civic_run(f: &Fixture, eval_run_id: &str, passed: u64, case_count: u64, status: EvalRunStatus) {
    let run = EvalRunRecord {
        eval_run_id: eval_run_id.to_string(),
        suite_id: CIVIC_EVAL_SUITE_ID.to_string(),
        subject_agent_id: AGENT_ID.to_string(),
        subject_revision_id: f.revision_id.clone(),
        baseline_revision_id: None,
        status,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: serde_json::json!({
            "suite_name": "Civic Core",
            "case_count": case_count,
            "passed": passed,
            "failed": case_count.saturating_sub(passed),
        }),
        report_handle: None,
        origin_node_id: "test-node".to_string(),
    };
    f.store.insert_eval_run(&run).unwrap();
}

fn promote(f: &Fixture, config: &GatewayConfig) -> serde_json::Value {
    let args = serde_json::json!({
        "agent_id": AGENT_ID,
        "revision_id": f.revision_id,
        "reason": "civic binding test",
    });
    let result = f
        .registry
        .execute(
            "agent_revision_promote",
            &f.proposer,
            &f.proposer_policy,
            &f.proposer_dir,
            Some(&f.gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-promote"),
            None,
            Some(config),
            Some(f.store.clone()),
            None,
        )
        .expect("execute should not hard-error");
    serde_json::from_str(&result).expect("response is JSON")
}

fn cfg_with_binding(enabled: bool, min_pass_ratio: f64) -> GatewayConfig {
    let mut cfg = GatewayConfig::default();
    cfg.require_operator_approval_for_new_agents = false;
    cfg.civic_eval_binding = CivicEvalBindingConfig {
        enabled,
        min_pass_ratio,
    };
    cfg
}

#[test]
#[serial_test::serial]
fn binding_disabled_promotes_with_low_score_advisory_only() {
    // Default config: binding off. A below-threshold civic run must NOT block
    // promotion; the advisory surface still reports the score.
    let f = setup();
    record_full_gate_pass(&f);
    seed_civic_run(&f, "eval-low-1", 1, 5, EvalRunStatus::Failed); // 0.2

    let cfg = cfg_with_binding(false, 0.8);
    let json = promote(&f, &cfg);

    assert_eq!(json["ok"], true, "binding disabled → promote succeeds: {json}");
    assert_eq!(json["status"], "promoted");
    assert_eq!(json["civic_eval_advisory"]["binding"], "disabled");
}

#[test]
#[serial_test::serial]
fn binding_enabled_below_threshold_rejected() {
    let f = setup();
    record_full_gate_pass(&f);
    seed_civic_run(&f, "eval-below-1", 3, 5, EvalRunStatus::Failed); // 0.6 < 0.8

    let cfg = cfg_with_binding(true, 0.8);
    let json = promote(&f, &cfg);

    assert_eq!(json["ok"], false, "binding enabled + below → must reject: {json}");
    assert_eq!(json["error_type"], "permission", "got: {json}");
    assert_eq!(json["error"], "civic_eval_below_threshold", "got: {json}");
    assert!(
        json["message"]
            .as_str()
            .unwrap_or_default()
            .contains("civic_eval_binding"),
        "got: {json}"
    );
    assert!(json["message"].as_str().unwrap_or_default().contains("0.600"));
    // The revision must NOT have been promoted: either no alias exists yet
    // (new agent) or the alias still points elsewhere. Either way, it must
    // not point at the rejected candidate.
    if let Some(alias) = f.store.get_agent_alias(AGENT_ID).unwrap() {
        assert_ne!(
            alias.revision_id, f.revision_id,
            "rejected promotion must not install the candidate revision"
        );
    }
}

#[test]
#[serial_test::serial]
fn binding_enabled_at_threshold_promotes_and_reports_passed() {
    let f = setup();
    record_full_gate_pass(&f);
    seed_civic_run(&f, "eval-pass-1", 4, 5, EvalRunStatus::Passed); // 0.8 == threshold

    let cfg = cfg_with_binding(true, 0.8);
    let json = promote(&f, &cfg);

    assert_eq!(json["ok"], true, "at-threshold → promote succeeds: {json}");
    assert_eq!(json["status"], "promoted");
    assert_eq!(json["civic_eval_advisory"]["binding"], "passed");
    assert_eq!(json["civic_eval_advisory"]["pass_ratio"], 0.8);
}

#[test]
#[serial_test::serial]
fn binding_enabled_missing_run_passes_through() {
    // Missing-run policy: if no completed civic eval run exists, promotion
    // proceeds (binding only bites when a run exists and fails).
    let f = setup();
    record_full_gate_pass(&f);
    // No seed_civic_run call.

    let cfg = cfg_with_binding(true, 0.8);
    let json = promote(&f, &cfg);

    assert_eq!(
        json["ok"], true,
        "missing run with binding on → pass-through: {json}"
    );
    assert_eq!(json["status"], "promoted");
    assert_eq!(json["civic_eval_advisory"]["binding"], "passed_no_run");
}
