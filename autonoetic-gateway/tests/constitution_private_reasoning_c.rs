//! Constitution Ri-0.13c: Capability-gated reasoning disclosure.
//!
//! Ri-0.13(c): Reasoning is disclosed to other parties only through a declared
//! capability (`ReasoningAudit`), and every disclosure writes a `reasoning.disclosed`
//! causal event the reviewed agent can see.
//!
//! Tests verify:
//! - Non-capability holder cannot read reasoning
//! - Capability holder can read reasoning
//! - Every access emits a `reasoning.disclosed` causal event
//! - Policy method `can_audit_reasoning` respects target patterns

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::runtime::tools::observability::ObservabilityReadReasoningTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use support::manifest_builder::TestManifest;

fn manifest_with_reasoning_audit(targets: Vec<&str>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "auditor.agent".to_string(),
            name: "Auditor Agent".to_string(),
            description: "test auditor".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ReasoningAudit {
            targets: targets.iter().map(|t| t.to_string()).collect(),
        }],
        ..TestManifest::new().build()
    }
}

fn manifest_without_audit() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "plain.agent".to_string(),
            name: "Plain Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..TestManifest::new().build()
    }
}

// ---------------------------------------------------------------------------
// Policy: can_audit_reasoning
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13c_policy_allows_wildcard_target() {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let policy = PolicyEngine::new(manifest);
    assert!(
        policy.can_audit_reasoning("any.agent").is_allowed(),
        "wildcard target must allow any agent"
    );
}

#[test]
fn ri_0_13c_policy_allows_prefix_match() {
    let manifest = manifest_with_reasoning_audit(vec!["auditor."]);
    let policy = PolicyEngine::new(manifest);
    assert!(
        policy.can_audit_reasoning("auditor.default").is_allowed(),
        "prefix match must work"
    );
    assert!(
        !policy.can_audit_reasoning("coder.default").is_allowed(),
        "non-matching agent must be denied"
    );
}

#[test]
fn ri_0_13c_policy_denies_without_capability() {
    let manifest = manifest_without_audit();
    let policy = PolicyEngine::new(manifest);
    assert!(
        !policy.can_audit_reasoning("any.agent").is_allowed(),
        "no ReasoningAudit capability must deny all"
    );
}

#[test]
fn ri_0_13c_policy_rule_id_is_ri_0_13() {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let policy = PolicyEngine::new(manifest);
    let allow = policy.can_audit_reasoning("any.agent");
    assert!(allow.enforced_rules.contains(&"Ri-0.13"));

    let manifest_no = manifest_without_audit();
    let policy_no = PolicyEngine::new(manifest_no);
    let deny = policy_no.can_audit_reasoning("denied.agent");
    assert!(deny.enforced_rules.contains(&"Ri-0.13"));
}

// ---------------------------------------------------------------------------
// Tool: is_available gates on ReasoningAudit
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13c_tool_available_with_capability() {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let tool = ObservabilityReadReasoningTool;
    assert!(
        tool.is_available(&manifest),
        "tool must be available with ReasoningAudit capability"
    );
}

#[test]
fn ri_0_13c_tool_unavailable_without_capability() {
    let manifest = manifest_without_audit();
    let tool = ObservabilityReadReasoningTool;
    assert!(
        !tool.is_available(&manifest),
        "tool must NOT be available without ReasoningAudit capability"
    );
}

// ---------------------------------------------------------------------------
// Tool: execute denies when capability doesn't cover target
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13c_execute_denies_uncovered_target() -> anyhow::Result<()> {
    let manifest = manifest_with_reasoning_audit(vec!["auditor."]);
    let policy = PolicyEngine::new(manifest.clone());
    let tempdir = tempfile::tempdir()?;

    let agents_dir = tempdir.path().join("agents");
    let auditor_dir = agents_dir.join("auditor.agent");
    std::fs::create_dir_all(&auditor_dir)?;

    let tool = ObservabilityReadReasoningTool;
    let result = tool.execute(
        &manifest,
        &policy,
        &auditor_dir,
        None,
        r#"{"target_session_id":"sess-1","target_agent_id":"coder.default"}"#,
        Some("caller-sess"),
        Some("turn-1"),
        None,
        None,
        None,
    )?;

    let json: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(json["ok"], false);
    assert_eq!(json["error_type"], "capability");
    Ok(())
}

#[test]
fn ri_0_13c_execute_rejects_path_traversal() -> anyhow::Result<()> {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let policy = PolicyEngine::new(manifest.clone());
    let tempdir = tempfile::tempdir()?;

    let agents_dir = tempdir.path().join("agents");
    let auditor_dir = agents_dir.join("auditor.agent");
    std::fs::create_dir_all(&auditor_dir)?;

    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let tool = ObservabilityReadReasoningTool;

    let result = tool.execute(
        &manifest,
        &policy,
        &auditor_dir,
        None,
        r#"{"target_session_id":"../../../etc","target_agent_id":"shadow"}"#,
        Some("caller-sess"),
        Some("turn-1"),
        None,
        Some(store.clone()),
        None,
    );
    let response = result.expect("registry returns structured envelope, not Rust Err");
    let parsed: serde_json::Value =
        serde_json::from_str(&response).expect("envelope must be JSON");
    assert_eq!(
        parsed["ok"], false,
        "path traversal in session_id must be rejected"
    );
    assert_eq!(
        parsed["error_type"], "validation",
        "rejection must use the validation envelope (P-5.11)"
    );

    let result2 = tool.execute(
        &manifest,
        &policy,
        &auditor_dir,
        None,
        r#"{"target_session_id":"ok","target_agent_id":"../evil"}"#,
        Some("caller-sess"),
        Some("turn-1"),
        None,
        Some(store.clone()),
        None,
    );
    let response2 = result2.expect("registry returns structured envelope, not Rust Err");
    let parsed2: serde_json::Value =
        serde_json::from_str(&response2).expect("envelope must be JSON");
    assert_eq!(
        parsed2["ok"], false,
        "path traversal in agent_id must be rejected"
    );
    assert_eq!(
        parsed2["error_type"], "validation",
        "rejection must use the validation envelope (P-5.11)"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Tool: execute reads reasoning and emits disclosure event
// ---------------------------------------------------------------------------

#[test]
fn ri_0_13c_execute_reads_and_discloses() -> anyhow::Result<()> {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let policy = PolicyEngine::new(manifest.clone());
    let tempdir = tempfile::tempdir()?;

    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agents_dir = tempdir.path().join("agents");
    let auditor_dir = agents_dir.join("auditor.agent");
    let target_dir = agents_dir.join("target.agent");
    std::fs::create_dir_all(&auditor_dir)?;
    std::fs::create_dir_all(&target_dir)?;

    let evidence_dir = target_dir
        .join("history")
        .join("evidence")
        .join("target-sess-1");
    std::fs::create_dir_all(&evidence_dir)?;

    let reasoning_file = evidence_dir.join("20260101T000000Z-turn-1-llm-reasoning-abc.json");
    std::fs::write(
        &reasoning_file,
        serde_json::to_string(&serde_json::json!({
            "reasoning_content": "I considered multiple approaches.",
            "reasoning_sha256": "abc123"
        }))?,
    )?;

    let tool = ObservabilityReadReasoningTool;
    let result = tool.execute(
        &manifest,
        &policy,
        &auditor_dir,
        None,
        r#"{"target_session_id":"target-sess-1","target_agent_id":"target.agent"}"#,
        Some("caller-sess"),
        Some("turn-1"),
        None,
        Some(store.clone()),
        None,
    )?;

    let json: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(json["ok"], true, "execute should succeed: {:?}", json);
    assert_eq!(json["count"], 1);
    assert_eq!(json["target_agent_id"], "target.agent");
    assert_eq!(json["target_session_id"], "target-sess-1");

    let entries = json["reasoning_entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["reasoning_sha256"], "abc123");

    let events = store.search_causal_events(Some("target-sess-1"), Some("target.agent"), 50)?;
    let disclosed = events
        .iter()
        .find(|e| e.category == "reasoning" && e.action == "disclosed");
    assert!(
        disclosed.is_some(),
        "reasoning.disclosed event must be emitted for the target agent"
    );
    let disclosed = disclosed.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(disclosed.payload.as_deref().expect("payload present"))?;
    assert_eq!(payload["reader_agent_id"], "auditor.agent");
    assert_eq!(payload["reader_session_id"], "caller-sess");
    assert_eq!(payload["entries_read"], 1);

    assert_eq!(
        disclosed.target.as_deref(),
        Some("auditor.agent"),
        "disclosure event target must be the reader, not the reviewed agent"
    );
    assert!(
        disclosed.enforced_rules.iter().any(|r| r == "Ri-0.13"),
        "disclosure event must cite Ri-0.13 in enforced_rules"
    );

    Ok(())
}

#[test]
fn ri_0_13c_execute_no_disclosure_when_no_capability() -> anyhow::Result<()> {
    let manifest = manifest_without_audit();
    let policy = PolicyEngine::new(manifest.clone());
    let tempdir = tempfile::tempdir()?;

    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agents_dir = tempdir.path().join("agents");
    let plain_dir = agents_dir.join("plain.agent");
    let target_dir = agents_dir.join("target.agent");
    std::fs::create_dir_all(&plain_dir)?;
    std::fs::create_dir_all(&target_dir)?;

    let evidence_dir = target_dir
        .join("history")
        .join("evidence")
        .join("target-sess-2");
    std::fs::create_dir_all(&evidence_dir)?;
    std::fs::write(
        evidence_dir.join("20260101T000000Z-turn-1-llm-reasoning-xyz.json"),
        serde_json::to_string(&serde_json::json!({
            "reasoning_content": "secret reasoning",
            "reasoning_sha256": "def456"
        }))?,
    )?;

    let tool = ObservabilityReadReasoningTool;
    let result = tool.execute(
        &manifest,
        &policy,
        &plain_dir,
        None,
        r#"{"target_session_id":"target-sess-2","target_agent_id":"target.agent"}"#,
        Some("plain-sess"),
        Some("turn-1"),
        None,
        Some(store.clone()),
        None,
    )?;

    let json: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(json["ok"], false);
    assert_eq!(json["error_type"], "capability");

    let events = store.search_causal_events(Some("target-sess-2"), Some("target.agent"), 50)?;
    let disclosed = events
        .iter()
        .find(|e| e.category == "reasoning" && e.action == "disclosed");
    assert!(
        disclosed.is_none(),
        "no reasoning.disclosed event must be emitted when capability is missing"
    );

    Ok(())
}

#[test]
fn ri_0_13c_execute_empty_when_no_evidence() -> anyhow::Result<()> {
    let manifest = manifest_with_reasoning_audit(vec!["*"]);
    let policy = PolicyEngine::new(manifest.clone());
    let tempdir = tempfile::tempdir()?;

    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agents_dir = tempdir.path().join("agents");
    let auditor_dir = agents_dir.join("auditor.agent");
    let target_dir = agents_dir.join("target.agent");
    std::fs::create_dir_all(&auditor_dir)?;
    std::fs::create_dir_all(&target_dir)?;

    let tool = ObservabilityReadReasoningTool;
    let result = tool.execute(
        &manifest,
        &policy,
        &auditor_dir,
        None,
        r#"{"target_session_id":"nonexistent-sess","target_agent_id":"target.agent"}"#,
        Some("caller-sess"),
        Some("turn-1"),
        None,
        Some(store.clone()),
        None,
    )?;

    let json: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(json["ok"], true);
    assert_eq!(json["count"], 0);
    assert!(
        json["message"]
            .as_str()
            .unwrap_or_default()
            .contains("No reasoning evidence"),
        "empty result should explain: {:?}",
        json
    );

    Ok(())
}

#[test]
fn ri_0_13c_tool_registered_in_default_registry() {
    let registry = default_registry();
    assert!(
        registry.has_tool("observability_read_reasoning"),
        "observability_read_reasoning must be in default registry"
    );
}
