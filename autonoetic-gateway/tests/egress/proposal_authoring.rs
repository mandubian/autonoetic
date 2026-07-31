//! `session.egress_policy.propose` — RFC §4.3 intent→proposal→confirm
//! authoring aid (#978). Covers the deterministic mapper over the real tool
//! catalog, the honest "nothing proposed" path for unknown subjects, the
//! no-effect-until-confirmed invariant (Lawful-Executor §14), and the
//! confirm-by-set round trip the room performs on one keystroke.

use crate::rpc_env::rpc_as;
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_gateway::runtime::egress_proposal::EgressProposal;
use autonoetic_types::egress::{label_display_name, EgressLabel, EgressSessionPolicy};

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("propose-test", method, params).await
}

async fn propose(intent: &str) -> EgressProposal {
    let resp = rpc(
        "session.egress_policy.propose",
        serde_json::json!({
            "session_id": "session-1",
            "intent": intent,
        }),
    )
    .await;
    assert!(
        resp.error.is_none(),
        "session.egress_policy.propose returned error: {:?}",
        resp.error
    );
    serde_json::from_value(resp.result.expect("propose result")).expect("proposal decodes")
}

/// `session.egress_policy.get`'s `policy` field is `null` when unset —
/// decode that as an empty policy.
fn policy_from_get(result: &serde_json::Value) -> EgressSessionPolicy {
    result
        .get("policy")
        .and_then(|p| if p.is_null() { None } else { Some(p) })
        .and_then(|p| serde_json::from_value(p.clone()).ok())
        .unwrap_or_default()
}

/// A tool-family subject resolves against the *real* registered catalog —
/// "wiki" maps to the wiki_* family glob; every proposed rule must actually
/// match at least one registered tool (no fabricated sources).
#[tokio::test]
async fn tool_family_subject_maps_to_matching_glob_rules() {
    let proposal = propose("keep wiki local").await;
    assert_eq!(proposal.kind, "tool");
    assert!(
        !proposal.rules.is_empty(),
        "expected wiki rules; note: {:?}",
        proposal.note
    );
    let catalog = autonoetic_gateway::runtime::tools::default_registry().registered_tool_names();
    for r in &proposal.rules {
        assert_eq!(
            r.rule.label,
            EgressLabel::local_only(),
            "proposals are always restrict-only local_only"
        );
        let matched: Vec<&String> = catalog
            .iter()
            .filter(|t| autonoetic_types::egress::source_pattern_matches(&r.rule.source, t))
            .collect();
        assert!(
            !matched.is_empty(),
            "rule {} matches no registered tool",
            r.rule.source
        );
        assert!(
            r.rationale.contains("registered tool"),
            "rule {} carries a reviewable rationale, got: {}",
            r.rule.source,
            r.rationale
        );
    }
    assert_eq!(
        label_display_name(&proposal.rules[0].rule.label),
        "local_only"
    );
}

/// An exact registered tool name yields a bare rule (no glob).
#[tokio::test]
async fn exact_tool_name_yields_bare_rule() {
    let proposal = propose("keep web_search local").await;
    assert!(!proposal.rules.is_empty());
    assert!(proposal.rules.iter().any(|r| r.rule.source == "web_search"));
}

/// A path subject yields the path-convention rule set (RFC §4.2's
/// `fs.read:~/mail/**` / `sandbox.exec:~/mail/**` shape).
#[tokio::test]
async fn path_subject_yields_path_convention_rules() {
    let proposal = propose("~/mail must not leave this machine").await;
    assert_eq!(proposal.kind, "path");
    let sources: Vec<&str> = proposal.rules.iter().map(|r| r.rule.source.as_str()).collect();
    assert!(sources.contains(&"fs.read"));
    assert!(sources.contains(&"sandbox.exec"));
    for r in &proposal.rules {
        assert_eq!(r.rule.path.as_deref(), Some("~/mail/**"));
        assert_eq!(r.rule.label, EgressLabel::local_only());
    }
}

/// An unknown subject proposes nothing — the mapper never fabricates a rule.
/// Near-miss sources surface for the operator to pick deliberately.
#[tokio::test]
async fn unknown_subject_proposes_nothing() {
    let proposal = propose("keep quantum-local-9 stays local").await;
    assert!(proposal.rules.is_empty());
    assert!(proposal.note.is_some());
}

/// Unrecognized phrasing is refused outright (the gateway cannot guess).
#[tokio::test]
async fn unrecognized_phrasing_is_refused() {
    let proposal = propose("emails should be deleted").await;
    assert!(proposal.rules.is_empty());
    assert!(proposal.note.is_some());
}

/// Empty session_id is a params error, never a silent proposal.
#[tokio::test]
async fn empty_session_id_is_invalid_params() {
    let resp = rpc(
        "session.egress_policy.propose",
        serde_json::json!({ "session_id": "  ", "intent": "emails stay local" }),
    )
    .await;
    assert_eq!(resp.error.as_ref().map(|e| e.code), Some(-32602));
}

/// The full RFC §4.3 loop over the real RPC surface: propose → the session
/// policy is untouched (a proposal has no effect) → confirm by set → the
/// declared rules are readable and valid. Restrict-only by construction.
#[tokio::test]
async fn propose_confirm_round_trip_declares_rules() {
    let session_id = "session-propose-1";
    let proposal = propose("keep wiki local").await;
    assert!(!proposal.rules.is_empty());

    // Unconfirmed proposal: no effect.
    let before = rpc(
        "session.egress_policy.get",
        serde_json::json!({ "session_id": session_id }),
    )
    .await;
    let before_policy = policy_from_get(&before.result.expect("get result"));
    assert!(before_policy.rules.is_empty());

    // Confirm: the room's one-keystroke `y` performs exactly this set.
    let confirm_policy = EgressSessionPolicy {
        rules: proposal.rules.iter().map(|p| p.rule.clone()).collect(),
        default_label: None,
        provider_constraint: None,
    };
    confirm_policy.validate().expect("proposed rules must validate");
    let resp = rpc(
        "session.egress_policy.set",
        serde_json::json!({
            "session_id": session_id,
            "policy": confirm_policy,
            "set_by": "operator:tui",
        }),
    )
    .await;
    assert!(resp.error.is_none(), "set failed: {:?}", resp.error);

    let after = rpc(
        "session.egress_policy.get",
        serde_json::json!({ "session_id": session_id }),
    )
    .await;
    let after_policy = policy_from_get(&after.result.expect("get result"));
    assert_eq!(after_policy.rules.len(), proposal.rules.len());
    for proposed in &proposal.rules {
        assert!(
            after_policy
                .rules
                .iter()
                .any(|r| r.source == proposed.rule.source && r.path == proposed.rule.path),
            "declared rules contain {}",
            proposed.rule.source
        );
    }
}

/// Same intent + same catalog ⇒ same proposal, byte-for-byte (the mapper is
/// deterministic, so a confirmed rule set is reproducible).
#[tokio::test]
async fn proposal_is_deterministic() {
    let a = propose("~/mail must not leave this machine").await;
    let b = propose("~/mail must not leave this machine").await;
    assert_eq!(
        serde_json::to_value(&a).unwrap(),
        serde_json::to_value(&b).unwrap()
    );
}
