//! Steward office — native amendment drafting (#773 Part F parity with the
//! ombudsman's `anomaly_adjudicate`).
//!
//! The steward office used to spawn `governance-author.default` to file
//! amendment proposals on its behalf — the drafting seat held the
//! `ConstitutionalProposal` capability, the steward only monitored. This
//! test pins the parity change: the steward manifest now holds the
//! capability itself (scoped to the constructive kinds), calls
//! `constitution_propose_amendment` directly, and the removal kinds stay
//! outside its grant — the deliberate second-seat path that still routes
//! through governance-author.
//!
//! The manifest under test is the REAL
//! `agents/evolution/evolution-steward.default/SKILL.md`, parsed through the
//! production `SkillParser` — so a future manifest edit that drops the
//! capability (breaking the office's filing path) or broadens the patterns
//! (weakening the removal-kind separation of duties) fails here immediately
//! rather than at runtime.


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use std::sync::Arc;

const STEWARD_SKILL_MD: &str =
    include_str!("../../../agents/evolution/evolution-steward.default/SKILL.md");

fn steward_manifest() -> AgentManifest {
    SkillParser::parse(STEWARD_SKILL_MD)
        .expect("steward SKILL.md should parse")
        .0
}

fn make_harness() -> (tempfile::TempDir, Arc<GatewayStore>, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("evolution-steward.default");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));
    (temp, store, agent_dir)
}

fn invoke(
    store: &Arc<GatewayStore>,
    agent_dir: &std::path::Path,
    manifest: &AgentManifest,
    args_json: &str,
) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let raw = registry
        .execute(
            "constitution_propose_amendment",
            manifest,
            &policy,
            agent_dir,
            None,
            args_json,
            Some("steward-sweep"),
            Some("turn-000001"),
            Some(&gateway_config),
            Some(store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn steward_manifest_holds_scoped_constitutional_proposal_capability() {
    let manifest = steward_manifest();
    let cap = manifest
        .capabilities
        .iter()
        .find_map(|c| match c {
            Capability::ConstitutionalProposal { patterns } => Some(patterns),
            _ => None,
        })
        .expect("steward manifest must declare ConstitutionalProposal");

    // Explicit constructive-kind list, never a wildcard — the removal kinds
    // stay behind governance-author as the second seat.
    for kind in ["add_rule", "modify_rule", "add_right", "modify_right"] {
        assert!(
            cap.iter().any(|p| p == kind),
            "steward grant should cover constructive kind {kind}"
        );
    }
    assert!(
        !cap.iter().any(|p| p == "*"),
        "steward grant must not be wildcard — removal kinds delegate to governance-author"
    );
    for kind in ["remove_rule", "remove_right"] {
        assert!(
            !cap.iter().any(|p| p == kind),
            "steward grant must not cover {kind} — that goes through the second seat"
        );
    }
}

#[test]
fn propose_amendment_tool_available_for_steward_manifest() {
    let manifest = steward_manifest();
    let registry = default_registry();
    let defs = registry.available_definitions(&manifest);
    assert!(
        defs.iter()
            .any(|d| d.name == "constitution_propose_amendment"),
        "constitution_propose_amendment must be available to the steward manifest"
    );
}

#[test]
fn steward_files_constructive_proposal_directly() {
    let (_temp, store, agent_dir) = make_harness();
    let manifest = steward_manifest();
    let resp = invoke(
        &store,
        &agent_dir,
        &manifest,
        r#"{
            "kind": "modify_rule",
            "target_id": "P-5.2",
            "proposed_text": "Input normalization tolerance narrowed to whitespace and casing; any structural repair is a named DISCRETION LEAK requiring explicit amendment.",
            "justification": "P-5.2 tops the DISCRETION LEAK register across three consecutive windows; the law should name the tolerance rather than leave the gateway improvising at the same site.",
            "evidence": ["evt-dleak-01", "evt-dleak-02"]
        }"#,
    );
    assert_eq!(resp["ok"], true, "constructive filing should succeed: {resp}");
    let proposal_id = resp["proposal_id"].as_str().expect("proposal_id present");
    assert!(proposal_id.starts_with("cprop-"), "durable id format");
    assert_eq!(resp["status"], "pending", "operator queue is the backstop");

    let row = store
        .get_constitutional_proposal(proposal_id)
        .expect("query")
        .expect("row exists");
    assert_eq!(row.kind, "modify_rule");
    assert_eq!(row.target_id.as_deref(), Some("P-5.2"));
    assert_eq!(row.status, "pending");
    assert_eq!(row.proposer_agent_id, "evolution-steward.default");
}

#[test]
fn steward_cannot_file_removal_proposals() {
    let (_temp, store, agent_dir) = make_harness();
    let manifest = steward_manifest();

    for kind in ["remove_rule", "remove_right"] {
        let target = if kind == "remove_rule" {
            "P-2.11"
        } else {
            "Ri-0.5"
        };
        let args = format!(
            r#"{{"kind": "{kind}", "target_id": "{target}", "justification": "should be rejected by the scoped grant"}}"#
        );
        let resp = invoke(&store, &agent_dir, &manifest, &args);
        assert_eq!(resp["ok"], false, "{kind} must be rejected");
        assert_eq!(resp["error_type"], "permission");
        let msg = resp["message"].as_str().unwrap_or_default();
        assert!(
            msg.contains(kind),
            "rejection should name the uncovered kind {kind}: {msg}"
        );
    }

    // No proposal row leaked through either rejection.
    assert!(
        store
            .list_pending_constitutional_proposals(None, 10)
            .expect("query")
            .is_empty(),
        "rejected removal filings must not persist"
    );
}
