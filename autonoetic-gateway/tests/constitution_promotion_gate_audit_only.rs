//! Constitution test for RFC scope 5.11 — gateway-side enforcement of the
//! `audit_only` promotion-gate mode.
//!
//! Pure-skill agents (reasoning-only with no high-risk capabilities) install
//! via an intent-only artifact bundle. `enforce_promotion_gate` in
//! `agent_revision.rs` must:
//!
//!   1. Reject promotion when no auditor `promotion_record` exists for the
//!      revision's artifact_id.
//!   2. Allow promotion when an auditor `promotion_record(pass=true)` is
//!      present — even with no evaluator record (audit_only mode).
//!   3. Reject promotion when the auditor identity equals the revision
//!      proposer's identity (P-2.17 reduced: auditor ≠ proposer for the
//!      audit_only mode).
//!   4. Reject promotion when the auditor's record has `pass=false`.
//!   5. **Not** weaken existing enforcement: a revision that declares
//!      `NetworkAccess` together with an intent-only artifact must still go
//!      through the full eval+audit gate (the `has_high_risk && artifact_id`
//!      branch unchanged).
//!
//! Refs: docs/archived/sealed-network-evaluation-plan.md §3.5.5 / scope 5.11.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::artifact::ArtifactKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::tempdir;
use support::manifest_builder::TestManifest;

// ── Helpers ────────────────────────────────────────────────────

fn intent_only_skill_md(agent_id: &str) -> String {
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
  description: "Pure-skill test agent, reasoning-only."
capabilities:
  - type: ReadAccess
    scopes:
      - "self.*"
  - type: WriteAccess
    scopes:
      - "self.*"
llm_config:
  provider: "openai"
  model: "test-model"
execution_mode: reasoning
---
# {agent_id}

You are a pure-skill agent. Respond to the user's request.
"#,
        agent_id = agent_id
    )
}

fn high_risk_intent_only_skill_md(agent_id: &str) -> String {
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
  description: "Reasoning-only agent that nonetheless declares NetworkAccess."
capabilities:
  - type: ReadAccess
    scopes:
      - "self.*"
  - type: NetworkAccess
    hosts:
      - "api.example.com"
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

/// Builds an intent-only artifact bundle: only a `<agent_id>.skill.md`
/// content, no entrypoints, no script_entry. Matches what agent-factory's
/// Step 2a produces under scope 5.10.
fn build_intent_only_artifact(
    base_dir: &Path,
    agent_id: &str,
    skill_md: &str,
) -> (String, PathBuf) {
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
            None, // no entrypoints — intent-only
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();
    (bundle.artifact_id, gateway_dir)
}

/// Identity for the agent that proposes installs (acts as
/// `rev.created_by_id` for the audit_only P-2.17-reduced check).
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

fn auditor_manifest(id: &str) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: id.to_string(),
            name: id.to_string(),
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

/// Create a revision via `agent_revision_create_from_intent` (the path the
/// real specialized_builder uses for intent-only installs under scope 5.10).
fn create_revision_from_intent(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    proposer_dir: &Path,
    gateway_dir: &Path,
    config: &GatewayConfig,
    store: Arc<GatewayStore>,
    agent_id: &str,
    artifact_id: &str,
    capabilities: serde_json::Value,
) -> String {
    let args = serde_json::json!({
        "agent_id": agent_id,
        "artifact_id": artifact_id,
        "instructions": format!("# {}\n\nTest agent body.", agent_id),
        "description": "Pure-skill test agent.",
        "capabilities": capabilities,
        "execution_mode": "reasoning",
        "llm_config": {
            "provider": "openai",
            "model": "test-model",
        },
    });
    let result = registry
        .execute(
            "agent_revision_create_from_intent",
            manifest,
            policy,
            proposer_dir,
            Some(gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some("session-create"),
            None,
            Some(config),
            Some(store),
            None,
        )
        .expect("revision create_from_intent should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    if parsed.get("ok") == Some(&serde_json::Value::Bool(false)) {
        panic!("create_from_intent returned an error envelope: {}", result);
    }
    parsed
        .get("revision_id")
        .and_then(|v| v.as_str())
        .expect("revision_id in response")
        .to_string()
}

fn record_promotion(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &Path,
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
            agent_dir,
            Some(gateway_dir),
            &serde_json::to_string(&args).unwrap(),
            Some(session_id),
            None,
            Some(config),
            Some(gw_store.clone()),
            None,
        )
        .expect("promotion_record should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promotion_record should return ok=true; got: {}",
        result
    );
}

fn try_promote(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &Path,
    gateway_dir: &Path,
    config: &GatewayConfig,
    store: Arc<GatewayStore>,
    agent_id: &str,
    revision_id: &str,
) -> Result<serde_json::Value, String> {
    let args = serde_json::json!({
        "agent_id": agent_id,
        "revision_id": revision_id,
        "reason": "5.11 constitution test",
    });
    match registry.execute(
        "agent_revision_promote",
        manifest,
        policy,
        agent_dir,
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
/// so the failure match arms below read naturally; a genuine success stays `Ok`.
fn as_outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) if v["ok"] == serde_json::Value::Bool(false) => {
            Err(v["message"].as_str().unwrap_or_default().to_string())
        }
        other => other,
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
    agent_id: String,
}

fn setup(agent_id: &str, skill_md: &str, capabilities: serde_json::Value) -> Fixture {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let proposer_dir = agents_dir.join("specialized_builder.default");
    std::fs::create_dir_all(&proposer_dir).expect("proposer dir");

    let (artifact_id, gateway_dir) = build_intent_only_artifact(temp.path(), agent_id, skill_md);

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        // Isolate the audit/eval completeness gate from the new-agent
        // first-admission human gate (covered by its own test).
        require_operator_approval_for_new_agents: false,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let registry = default_registry();
    let proposer = proposer_manifest();
    let proposer_policy = PolicyEngine::new(proposer.clone());

    let revision_id = create_revision_from_intent(
        &registry,
        &proposer,
        &proposer_policy,
        &proposer_dir,
        &gateway_dir,
        &config,
        store.clone(),
        agent_id,
        &artifact_id,
        capabilities,
    );

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
        agent_id: agent_id.to_string(),
    }
}

fn low_risk_caps() -> serde_json::Value {
    serde_json::json!([
        {"type": "ReadAccess", "scopes": ["self.*"]},
        {"type": "WriteAccess", "scopes": ["self.*"]},
    ])
}

fn high_risk_caps() -> serde_json::Value {
    serde_json::json!([
        {"type": "ReadAccess", "scopes": ["self.*"]},
        {"type": "NetworkAccess", "hosts": ["api.example.com"]},
    ])
}

// ── Tests ──────────────────────────────────────────────────────

#[test]
fn audit_only_promote_fails_without_auditor_record() {
    let f = setup(
        "pure.skill.no.audit",
        &intent_only_skill_md("pure.skill.no.audit"),
        low_risk_caps(),
    );

    let result = try_promote(
        &f.registry,
        &f.proposer,
        &f.proposer_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        f.store.clone(),
        &f.agent_id,
        &f.revision_id,
    );

    match as_outcome(result) {
        Err(msg) => {
            assert!(
                msg.contains("Promotion gate") && msg.contains("no auditor promotion.record"),
                "expected audit_only gate rejection naming auditor, got: {}",
                msg
            );
        }
        Ok(json) => panic!(
            "promote must fail without auditor record in audit_only mode, got: {}",
            json
        ),
    }
}

#[test]
fn audit_only_promote_succeeds_with_auditor_pass() {
    let f = setup(
        "pure.skill.ok",
        &intent_only_skill_md("pure.skill.ok"),
        low_risk_caps(),
    );

    let auditor = auditor_manifest("auditor.default");
    let auditor_policy = PolicyEngine::new(auditor.clone());
    record_promotion(
        &f.registry,
        &auditor,
        &auditor_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        &f.store,
        &f.artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    let result = try_promote(
        &f.registry,
        &f.proposer,
        &f.proposer_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        f.store.clone(),
        &f.agent_id,
        &f.revision_id,
    );

    let json = result.expect("audit_only promote must succeed with auditor pass");
    assert_eq!(
        json.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "promote must return ok=true: {}",
        json
    );
    assert_eq!(
        json.get("status").and_then(|v| v.as_str()),
        Some("promoted"),
        "promote must transition revision to promoted: {}",
        json
    );
}

#[test]
fn audit_only_promote_fails_with_auditor_record_pass_false() {
    let f = setup(
        "pure.skill.audit.fail",
        &intent_only_skill_md("pure.skill.audit.fail"),
        low_risk_caps(),
    );

    let auditor = auditor_manifest("auditor.default");
    let auditor_policy = PolicyEngine::new(auditor.clone());
    record_promotion(
        &f.registry,
        &auditor,
        &auditor_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        &f.store,
        &f.artifact_id,
        "auditor",
        false, // auditor explicitly rejected the install
        "session-auditor-fail",
    );

    let result = try_promote(
        &f.registry,
        &f.proposer,
        &f.proposer_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        f.store.clone(),
        &f.agent_id,
        &f.revision_id,
    );

    match as_outcome(result) {
        Err(msg) => assert!(
            msg.contains("Promotion gate") && msg.contains("auditor did not pass"),
            "expected auditor-fail rejection, got: {}",
            msg
        ),
        Ok(json) => panic!(
            "promote must fail when auditor record carries pass=false, got: {}",
            json
        ),
    }
}

#[test]
fn audit_only_promote_fails_when_auditor_is_proposer_self_approval() {
    // P-2.17 reduced for audit_only: the auditor identity must differ from
    // the revision's proposer (the agent that created the revision).
    //
    // The promotion_record tool gates by `is_promotion_agent` (only
    // evaluator.default and auditor.default may call it), so in normal
    // operation a proposer like specialized_builder.default cannot record
    // an audit for itself. This test exercises the *gate-level* defence —
    // we bypass `is_promotion_agent` by writing directly to PromotionStore
    // with `agent_id = rev.created_by_id` to simulate a scenario where the
    // outer guard has been compromised. The gate must still reject.

    let f = setup(
        "pure.skill.self.audit",
        &intent_only_skill_md("pure.skill.self.audit"),
        low_risk_caps(),
    );

    // Write the audit record directly, using the proposer's identity.
    let promo_store =
        autonoetic_gateway::runtime::promotion_store::PromotionStore::new(&f.gateway_dir)
            .expect("open promotion store");
    let rev_record = f
        .store
        .get_agent_revision(&f.revision_id)
        .expect("load revision")
        .expect("revision exists");
    promo_store
        .record_promotion(
            f.artifact_id.clone(),
            None,
            Some(rev_record.content_digest.clone()),
            autonoetic_types::promotion::PromotionRole::Auditor,
            &f.proposer.agent.id, // == rev.created_by_id, the self-approval attack
            true,
            vec![],
            Some("Self-approval test".to_string()),
            None,
        )
        .expect("write self-audit record");

    let result = try_promote(
        &f.registry,
        &f.proposer,
        &f.proposer_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        f.store.clone(),
        &f.agent_id,
        &f.revision_id,
    );

    match as_outcome(result) {
        Err(msg) => {
            assert!(
                msg.contains("P-2.17") && msg.contains("same identity that proposed"),
                "expected P-2.17 audit_only self-approval rejection, got: {}",
                msg
            );
        }
        Ok(json) => panic!(
            "promote must reject self-approval (auditor == proposer) in audit_only mode, got: {}",
            json
        ),
    }
}

#[test]
fn high_risk_intent_only_artifact_still_requires_full_gate() {
    // Regression pin: an intent-only artifact that declares NetworkAccess
    // must still go through the full eval + audit gate. The audit_only
    // branch must not weaken existing high-risk enforcement.
    let f = setup(
        "high.risk.intent.only",
        &high_risk_intent_only_skill_md("high.risk.intent.only"),
        high_risk_caps(),
    );

    // Only audit, no evaluator.
    let auditor = auditor_manifest("auditor.default");
    let auditor_policy = PolicyEngine::new(auditor.clone());
    record_promotion(
        &f.registry,
        &auditor,
        &auditor_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        &f.store,
        &f.artifact_id,
        "auditor",
        true,
        "session-auditor",
    );

    let result = try_promote(
        &f.registry,
        &f.proposer,
        &f.proposer_policy,
        &f.proposer_dir,
        &f.gateway_dir,
        &f.config,
        f.store.clone(),
        &f.agent_id,
        &f.revision_id,
    );

    match as_outcome(result) {
        Err(msg) => {
            assert!(
                msg.contains("no evaluator role passed") || msg.contains("no promotion.record"),
                "high-risk intent-only must still require evaluator pass, got: {}",
                msg
            );
        }
        Ok(json) => panic!(
            "high-risk intent-only must require evaluator + auditor (full mode), got: {}",
            json
        ),
    }
}
