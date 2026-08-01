//! `egress.lineage` operator RPC (#975, RFC §9.1) — "why is this tainted,
//! since when, and from what?"
//!
//! The label plane could show *that* a session was tainted and never *why*.
//! These tests build the RFC §5.6 email chain in the causal store and assert the
//! walk reconstructs it: turn 4's `sandbox_exec` matched a path rule, and
//! argument taint carried that label into the turns after it.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_types::causal_chain::CausalEventRecord;
use autonoetic_types::egress::EgressLabel;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("lineage-test", method, params).await
}

async fn lineage(params: serde_json::Value) -> serde_json::Value {
    let resp = rpc("egress.lineage", params).await;
    assert!(
        resp.error.is_none(),
        "egress.lineage returned error: {:?}",
        resp.error
    );
    resp.result.expect("egress.lineage result")
}

/// Seed one `egress.envelope_labeled` event in the shape the runtime emitter
/// produces (`egress_labeler::emit_envelope_labeled_event`).
#[allow(clippy::too_many_arguments)]
fn seed_envelope(
    session_id: &str,
    seq: u64,
    envelope_id: &str,
    tool_call_id: &str,
    turn_id: &str,
    resolution: &str,
    matched_rules: &[&str],
    parents: &[&str],
) {
    let payload = serde_json::json!({
        "envelope_id": envelope_id,
        "tool_call_id": tool_call_id,
        "tool_name": "sandbox_exec",
        "label": EgressLabel::local_only(),
        "resolution": resolution,
        "matched_rules": matched_rules,
        "matched_rule_scopes": matched_rules
            .iter()
            .map(|r| serde_json::json!({ "rule": r, "scope": "global" }))
            .collect::<Vec<_>>(),
        "parent_envelope_ids": parents,
        "taint_applied": !parents.is_empty(),
        "artifact_labels_applied": [],
        "bundle_floor_applied": false,
    });
    env()
        .store
        .create_causal_event(&CausalEventRecord {
            event_id: format!("ev-{session_id}-{seq}"),
            agent_id: "coder.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: Some(turn_id.to_string()),
            event_seq: seq,
            timestamp: "2026-08-01T00:00:00Z".to_string(),
            category: "egress".to_string(),
            action: "egress.envelope_labeled".to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: Some(envelope_id.to_string()),
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: Some("egress_label_resolved".to_string()),
        })
        .expect("seed envelope event");
}

/// Build the RFC §5.6 chain: a path-rule origin at turn 4, then two hops of
/// argument taint.
fn seed_email_chain(sid: &str) {
    seed_envelope(
        sid,
        1,
        "env_origin",
        "tc_mail_read",
        "turn-000004",
        "operator_rule",
        &["sandbox.exec:~/mail/**"],
        &[],
    );
    seed_envelope(
        sid,
        2,
        "env_mid",
        "tc_parse",
        "turn-000005",
        "default",
        &[],
        &["tc_mail_read"],
    );
    seed_envelope(
        sid,
        3,
        "env_leaf",
        "tc_summarize",
        "turn-000007",
        "default",
        &[],
        &["tc_parse"],
    );
}

#[tokio::test]
async fn lineage_walks_argument_taint_back_to_the_rule_that_started_it() {
    let sid = "sess-lineage-chain";
    seed_email_chain(sid);

    let v = lineage(serde_json::json!({
        "root_session_id": sid,
        "from": "env_leaf",
    }))
    .await;

    let nodes = v["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 3, "leaf → mid → origin");

    // Nearest-first, with depth counting hops from the queried envelope.
    assert_eq!(nodes[0]["envelope_id"], "env_leaf");
    assert_eq!(nodes[0]["depth"], 0);
    assert_eq!(nodes[0]["origin"], "argument_taint");
    assert_eq!(nodes[1]["envelope_id"], "env_mid");
    assert_eq!(nodes[1]["depth"], 1);
    assert_eq!(nodes[2]["envelope_id"], "env_origin");
    assert_eq!(nodes[2]["depth"], 2);

    // The origin is the actionable end of the chain: the rule an operator wrote.
    assert_eq!(nodes[2]["origin"], "operator_rule");
    assert_eq!(nodes[2]["matched_rules"][0], "sandbox.exec:~/mail/**");
    assert_eq!(nodes[2]["turn_id"], "turn-000004");
    assert_eq!(nodes[2]["is_origin"], true);

    assert_eq!(
        v["origins"].as_array().unwrap(),
        &vec![serde_json::json!("env_origin")],
        "exactly one origin explains the whole chain"
    );
}

/// An operator reading an audit row has a tool-call id, not an `env_*` id.
/// Both must work, or the RPC is unusable from the surface that feeds it.
#[tokio::test]
async fn from_accepts_either_an_envelope_id_or_a_tool_call_id() {
    let sid = "sess-lineage-idspace";
    seed_email_chain(sid);

    let by_env = lineage(serde_json::json!({ "root_session_id": sid, "from": "env_leaf" })).await;
    let by_tcid =
        lineage(serde_json::json!({ "root_session_id": sid, "from": "tc_summarize" })).await;
    assert_eq!(
        by_env["nodes"], by_tcid["nodes"],
        "the same chain, whichever id space the caller has"
    );
}

/// Omitting `from` asks the session-level question and must start from every
/// restricted envelope — otherwise "why is this room tainted?" has no answer.
#[tokio::test]
async fn omitting_from_walks_the_whole_session() {
    let sid = "sess-lineage-session";
    seed_email_chain(sid);

    let v = lineage(serde_json::json!({ "root_session_id": sid })).await;
    let roots = v["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 3, "all three restricted envelopes are starts");
    // Still exactly one origin — the walk dedups shared ancestry rather than
    // reporting the origin once per path that reaches it.
    assert_eq!(
        v["origins"].as_array().unwrap(),
        &vec![serde_json::json!("env_origin")]
    );
    assert_eq!(v["nodes"].as_array().unwrap().len(), 3, "no node repeats");
}

/// A parent outside the scanned window must not be reported as a true origin
/// without the caller being able to tell — that would answer "where did this
/// come from" with a confident wrong answer.
#[tokio::test]
async fn a_cut_chain_is_flagged_truncated() {
    let sid = "sess-lineage-cut";
    seed_email_chain(sid);

    // limit below the seeded count → the window fills and the chain is cut.
    let v = lineage(serde_json::json!({ "root_session_id": sid, "limit": 1 })).await;
    assert_eq!(v["truncated"], true);
    assert_eq!(v["limit"], 1);
}

/// An unknown `from` is an empty walk, not an error and not a fabricated node.
#[tokio::test]
async fn unknown_from_yields_an_empty_lineage() {
    let sid = "sess-lineage-unknown";
    seed_email_chain(sid);
    let v = lineage(serde_json::json!({ "root_session_id": sid, "from": "env_nope" })).await;
    assert_eq!(v["nodes"].as_array().unwrap().len(), 0);
    assert_eq!(v["roots"].as_array().unwrap().len(), 0);
}

/// A self-referencing parent must terminate. Argument-taint ids are
/// gateway-minted so this should not arise, but a walk that can hang on
/// malformed history is not a walk anyone should run against a live store.
#[tokio::test]
async fn a_cyclic_parent_reference_terminates() {
    let sid = "sess-lineage-cycle";
    seed_envelope(
        sid,
        1,
        "env_a",
        "tc_a",
        "turn-000001",
        "default",
        &[],
        &["tc_b"],
    );
    seed_envelope(
        sid,
        2,
        "env_b",
        "tc_b",
        "turn-000002",
        "default",
        &[],
        &["tc_a"],
    );

    let v = lineage(serde_json::json!({ "root_session_id": sid, "from": "env_a" })).await;
    assert_eq!(
        v["nodes"].as_array().unwrap().len(),
        2,
        "each envelope visited once, then the walk stops"
    );
}

#[tokio::test]
async fn oversized_limit_is_clamped_and_echoed() {
    let sid = "sess-lineage-clamp";
    seed_email_chain(sid);
    let v = lineage(serde_json::json!({
        "root_session_id": sid,
        "limit": 900_000_000_i64,
    }))
    .await;
    assert_eq!(
        v["limit"],
        autonoetic_gateway::egress_lineage::MAX_LINEAGE_LIMIT,
        "a remote caller must not be able to widen the scan"
    );
}

#[tokio::test]
async fn empty_root_session_id_is_invalid_params() {
    let resp = rpc(
        "egress.lineage",
        serde_json::json!({ "root_session_id": "  " }),
    )
    .await;
    let err = resp.error.expect("expected an error");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("root_session_id"));
}

/// Metadata only: the lineage is safe on the wire precisely because it carries
/// ids, labels and rule names — never content.
#[tokio::test]
async fn lineage_carries_no_content_keys() {
    let sid = "sess-lineage-metaonly";
    seed_email_chain(sid);
    let v = lineage(serde_json::json!({ "root_session_id": sid })).await;
    let wire = serde_json::to_string(&v).expect("serialize");
    for key in ["\"content\"", "\"stdout\"", "\"stderr\"", "\"message\""] {
        assert!(!wire.contains(key), "lineage leaked a content key {key}");
    }
}
