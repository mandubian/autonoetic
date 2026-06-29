//! Constitution Ri-0.8 / R+++1 — Amendment proposal channel for agents (issue #92).
//!
//! Verifies the `constitution_propose_amendment` native tool and the
//! supporting persistence layer:
//!   - rejected when the agent does not hold `ConstitutionalProposal`
//!   - rejected when the held capability does not cover the requested kind
//!   - accepted with capability, persisted with a durable ID, status `pending`
//!   - operator approve / reject / defer transitions the row
//!   - release publication marks approved-but-unpublished rows with the tag
//!   - kind-specific arg validation (target_id / proposed_text)
//!   - validation rejects justification when empty.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use std::sync::Arc;
use tempfile::tempdir;

fn manifest_with_capabilities(caps: Vec<Capability>) -> AgentManifest {
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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
        },
        capabilities: caps,
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

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
}

fn make_harness() -> Harness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("test-agent");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));
    Harness {
        _temp: temp,
        store,
        agent_dir,
    }
}

fn invoke(h: &Harness, manifest: &AgentManifest, args_json: &str) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let raw = registry
        .execute(
            "constitution_propose_amendment",
            manifest,
            &policy,
            &h.agent_dir,
            None,
            args_json,
            Some("test-session"),
            Some("turn-000001"),
            Some(&gateway_config),
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn tool_unavailable_without_capability() {
    let registry = default_registry();
    let manifest = manifest_with_capabilities(vec![]);
    let defs = registry.available_definitions(&manifest);
    assert!(
        !defs
            .iter()
            .any(|d| d.name == "constitution_propose_amendment"),
        "tool must be hidden from agents without ConstitutionalProposal capability"
    );
}

#[test]
fn tool_available_with_capability() {
    let registry = default_registry();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);
    let defs = registry.available_definitions(&manifest);
    assert!(
        defs.iter()
            .any(|d| d.name == "constitution_propose_amendment"),
        "tool must be available when ConstitutionalProposal is declared"
    );
}

#[test]
fn capability_holder_proposal_persists_with_durable_id() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);
    let resp = invoke(
        &h,
        &manifest,
        r#"{
            "kind": "modify_rule",
            "target_id": "P-7.5",
            "proposed_text": "Approval flood cap raised from 10 to 50 per root session.",
            "justification": "Throughput-limited workflows hit the cap during legitimate burst review.",
            "evidence": ["evt-aaaa", "evt-bbbb"]
        }"#,
    );
    assert_eq!(resp["ok"], true);
    let proposal_id = resp["proposal_id"].as_str().expect("proposal_id present");
    assert!(proposal_id.starts_with("cprop-"), "durable id format");
    assert_eq!(resp["status"], "pending");
    assert!(resp["constitution_digest"].is_string());

    let row = h
        .store
        .get_constitutional_proposal(proposal_id)
        .expect("query")
        .expect("row exists");
    assert_eq!(row.kind, "modify_rule");
    assert_eq!(row.target_id.as_deref(), Some("P-7.5"));
    assert_eq!(row.status, "pending");
    assert_eq!(row.proposer_agent_id, "test-agent");
    assert_eq!(row.proposer_session_id.as_deref(), Some("test-session"));
    assert!(matches!(
        row.evidence_json,
        serde_json::Value::Array(ref a) if a.len() == 2
    ));
}

#[test]
fn pattern_restricted_capability_rejects_unmatched_kind() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["modify_rule".to_string()],
    }]);
    let resp = invoke(
        &h,
        &manifest,
        r#"{
            "kind": "remove_right",
            "target_id": "Ri-0.5",
            "justification": "test"
        }"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "permission");
    let msg = resp["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("remove_right"),
        "error names the rejected kind: {}",
        msg
    );

    // Nothing persisted.
    let listed = h
        .store
        .list_constitutional_proposals(None, None, 100)
        .unwrap();
    assert!(listed.is_empty(), "rejected proposal must not be stored");
}

#[test]
fn unknown_kind_returns_validation_error() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);
    let resp = invoke(
        &h,
        &manifest,
        r#"{"kind":"abolish_gateway","justification":"x"}"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}

#[test]
fn modify_kind_requires_target_and_text() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);

    let no_target = invoke(
        &h,
        &manifest,
        r#"{"kind":"modify_rule","proposed_text":"new text","justification":"j"}"#,
    );
    assert_eq!(no_target["ok"], false);
    assert!(no_target["message"]
        .as_str()
        .unwrap_or_default()
        .contains("target_id"));

    let no_text = invoke(
        &h,
        &manifest,
        r#"{"kind":"modify_rule","target_id":"P-7.5","justification":"j"}"#,
    );
    assert_eq!(no_text["ok"], false);
    assert!(no_text["message"]
        .as_str()
        .unwrap_or_default()
        .contains("proposed_text"));
}

#[test]
fn empty_justification_rejected() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);
    let resp = invoke(
        &h,
        &manifest,
        r#"{"kind":"add_rule","proposed_text":"new rule","justification":"   "}"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}

#[test]
fn operator_approve_reject_defer_transitions() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);

    // Submit three proposals.
    let make = |kind: &str, target: &str| -> String {
        let resp = invoke(
            &h,
            &manifest,
            &format!(
                r#"{{"kind":"{}","target_id":"{}","proposed_text":"x","justification":"j"}}"#,
                kind, target
            ),
        );
        resp["proposal_id"].as_str().unwrap().to_string()
    };
    let p_approve = make("modify_rule", "P-1.1");
    let p_reject = make("modify_rule", "P-1.2");
    let p_defer = make("modify_rule", "P-1.3");

    assert!(h
        .store
        .decide_constitutional_proposal(&p_approve, "approved", "alice", Some("LGTM"))
        .unwrap());
    assert!(h
        .store
        .decide_constitutional_proposal(&p_reject, "rejected", "alice", Some("scope creep"))
        .unwrap());
    assert!(h
        .store
        .decide_constitutional_proposal(&p_defer, "deferred", "alice", None)
        .unwrap());

    let approved = h
        .store
        .get_constitutional_proposal(&p_approve)
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, "approved");
    assert_eq!(approved.operator_decision.as_deref(), Some("approved"));
    assert_eq!(approved.decision_reason.as_deref(), Some("LGTM"));
    assert!(approved.decided_at.is_some());

    let rejected = h
        .store
        .get_constitutional_proposal(&p_reject)
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status, "rejected");

    let deferred = h
        .store
        .get_constitutional_proposal(&p_defer)
        .unwrap()
        .unwrap();
    assert_eq!(deferred.status, "deferred");
    assert!(deferred.decision_reason.is_none());

    // List filter by status.
    let only_approved = h
        .store
        .list_constitutional_proposals(Some("approved"), None, 100)
        .unwrap();
    assert_eq!(only_approved.len(), 1);
    assert_eq!(only_approved[0].proposal_id, p_approve);
}

#[test]
fn under_review_transition_does_not_record_decision() {
    // `under_review` is the non-terminal review-start status. It must not
    // stamp `operator_decision` / `decided_at` — those fields are reserved
    // for terminal decisions (approved / rejected / deferred).
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);
    let resp = invoke(
        &h,
        &manifest,
        r#"{"kind":"modify_rule","target_id":"P-1.1","proposed_text":"x","justification":"j"}"#,
    );
    let id = resp["proposal_id"].as_str().unwrap().to_string();

    assert!(h
        .store
        .decide_constitutional_proposal(&id, "under_review", "alice", Some("queueing"))
        .unwrap());

    let row = h.store.get_constitutional_proposal(&id).unwrap().unwrap();
    assert_eq!(row.status, "under_review");
    assert!(
        row.operator_decision.is_none(),
        "under_review must not stamp operator_decision"
    );
    assert!(
        row.decided_at.is_none(),
        "under_review must not stamp decided_at"
    );
    assert!(row.decided_by.is_none());
    assert!(row.decision_reason.is_none());

    // A subsequent terminal transition does stamp the decision fields.
    assert!(h
        .store
        .decide_constitutional_proposal(&id, "approved", "alice", Some("LGTM"))
        .unwrap());
    let row2 = h.store.get_constitutional_proposal(&id).unwrap().unwrap();
    assert_eq!(row2.status, "approved");
    assert_eq!(row2.operator_decision.as_deref(), Some("approved"));
    assert!(row2.decided_at.is_some());
    assert_eq!(row2.decided_by.as_deref(), Some("alice"));
    assert_eq!(row2.decision_reason.as_deref(), Some("LGTM"));
}

#[test]
fn release_marks_only_approved_unpublished() {
    let h = make_harness();
    let manifest = manifest_with_capabilities(vec![Capability::ConstitutionalProposal {
        patterns: vec!["*".to_string()],
    }]);

    let make = |kind: &str, target: &str| -> String {
        let resp = invoke(
            &h,
            &manifest,
            &format!(
                r#"{{"kind":"{}","target_id":"{}","proposed_text":"x","justification":"j"}}"#,
                kind, target
            ),
        );
        resp["proposal_id"].as_str().unwrap().to_string()
    };
    let p_a = make("modify_rule", "P-1.1");
    let p_b = make("modify_rule", "P-1.2");
    let p_c = make("modify_rule", "P-1.3");

    h.store
        .decide_constitutional_proposal(&p_a, "approved", "op", None)
        .unwrap();
    h.store
        .decide_constitutional_proposal(&p_b, "approved", "op", None)
        .unwrap();
    h.store
        .decide_constitutional_proposal(&p_c, "rejected", "op", None)
        .unwrap();

    let published = h.store.publish_approved_proposals("2026-Q2").unwrap();
    assert_eq!(published.len(), 2);
    assert!(published.contains(&p_a));
    assert!(published.contains(&p_b));

    // Re-running should publish nothing — already tagged.
    let again = h.store.publish_approved_proposals("2026-Q3").unwrap();
    assert!(
        again.is_empty(),
        "approved+published rows must not be re-tagged"
    );

    let rec_a = h.store.get_constitutional_proposal(&p_a).unwrap().unwrap();
    assert_eq!(rec_a.published_in_release.as_deref(), Some("2026-Q2"));
    let rec_c = h.store.get_constitutional_proposal(&p_c).unwrap().unwrap();
    assert!(
        rec_c.published_in_release.is_none(),
        "rejected proposals stay unpublished"
    );
}
