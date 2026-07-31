//! `egress.audit` operator RPC (#973, RFC §9.3).
//!
//! The per-turn audit was CLI-only: `handle_gateway_egress_audit` opened
//! `GatewayStore` directly, so the session room (a JSON-RPC client by design)
//! and every remote operator were locked out of the richest egress view that
//! exists. These tests cover the RPC's contract — per-turn grouping, the
//! `truncated` honesty flag, param validation, and the metadata-only invariant
//! that makes returning the report over the wire safe at all.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_types::causal_chain::CausalEventRecord;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("audit-test", method, params).await
}

async fn audit(params: serde_json::Value) -> serde_json::Value {
    let resp = rpc("egress.audit", params).await;
    assert!(
        resp.error.is_none(),
        "egress.audit returned error: {:?}",
        resp.error
    );
    resp.result.expect("egress.audit result")
}

/// Seed one egress causal event for a session/turn.
fn seed(session_id: &str, turn: Option<&str>, seq: u64, action: &str, payload: serde_json::Value) {
    env()
        .store
        .create_causal_event(&CausalEventRecord {
            event_id: format!("ev-{session_id}-{seq}"),
            agent_id: "coder.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: turn.map(str::to_string),
            event_seq: seq,
            timestamp: "2026-07-31T00:00:00Z".to_string(),
            category: "egress".to_string(),
            action: action.to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: None,
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        })
        .expect("seed causal event");
}

/// A non-egress event in the same session, to prove the handler filters by
/// category rather than returning the whole chain.
fn seed_noise(session_id: &str, seq: u64) {
    env()
        .store
        .create_causal_event(&CausalEventRecord {
            event_id: format!("ev-noise-{session_id}-{seq}"),
            agent_id: "coder.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: Some("t1".to_string()),
            event_seq: seq,
            timestamp: "2026-07-31T00:00:00Z".to_string(),
            category: "tool".to_string(),
            action: "tool.completed".to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: None,
            payload: Some(serde_json::json!({"stdout": "SECRET-CONTENT"}).to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        })
        .expect("seed noise event");
}

#[tokio::test]
async fn audit_groups_events_by_turn_and_tallies_totals() {
    let sid = "sess-audit-basic";
    seed(
        sid,
        Some("t1"),
        1,
        "egress.envelope_labeled",
        serde_json::json!({
            "tool_name": "sandbox_exec",
            "label_name": "local_only",
            "resolution": "operator_rule",
        }),
    );
    seed(
        sid,
        Some("t1"),
        2,
        "egress.envelope_withheld",
        serde_json::json!({
            "tool_call_id": "tc_1",
            "target_sink": "remote_model",
            "indication": "[withheld: 1× sandbox_exec result — policy local_only]",
        }),
    );
    seed(
        sid,
        Some("t1"),
        3,
        "egress.request_filtered",
        serde_json::json!({
            "preset": "sonnet",
            "target_sink": "remote_model",
            "withheld_count": 1,
            "included_count": 4,
            "violation_count": 0,
        }),
    );
    seed(
        sid,
        Some("t2"),
        4,
        "egress.boundary_refused",
        serde_json::json!({ "surface": "sandbox", "reason": "share_net with local_only band" }),
    );
    seed_noise(sid, 5);

    let v = audit(serde_json::json!({ "session_id": sid })).await;

    assert_eq!(v["report"]["session_id"], sid);
    assert_eq!(v["truncated"], false);
    assert!(v["limit"].as_i64().unwrap() > 0, "limit is echoed back");

    let turns = v["report"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "two turns seeded, two turns reported");
    assert_eq!(turns[0]["turn_id"], "t1");
    assert_eq!(
        turns[0]["rows"].as_array().unwrap().len(),
        3,
        "the non-egress event must not appear as a row"
    );
    assert_eq!(turns[1]["turn_id"], "t2");

    assert_eq!(v["report"]["total_withheld"], 1);
    assert_eq!(v["report"]["total_violations"], 0);
    assert_eq!(v["report"]["total_boundary_refusals"], 1);
}

/// The RPC and the CLI must not be able to disagree: both go through
/// `load_egress_audit`, so the same session yields the same report.
#[tokio::test]
async fn rpc_report_matches_the_shared_loader() {
    let sid = "sess-audit-parity";
    seed(
        sid,
        Some("t1"),
        1,
        "egress.request_filtered",
        serde_json::json!({
            "preset": "local",
            "target_sink": "local_model",
            "withheld_count": 2,
            "included_count": 1,
            "violation_count": 1,
        }),
    );

    let via_rpc = audit(serde_json::json!({ "session_id": sid })).await;
    let direct =
        autonoetic_gateway::egress_audit::load_egress_audit(env().store.as_ref(), sid, None)
            .expect("direct load");

    assert_eq!(
        via_rpc,
        serde_json::to_value(&direct).expect("serialize direct"),
        "the RPC must return exactly what the shared loader produces"
    );
}

/// A session that never labeled anything reports an empty audit — not an error.
/// "Nothing was restricted" is a real answer and must be distinguishable from a
/// failed read (which surfaces as an RPC error).
#[tokio::test]
async fn session_without_egress_events_reports_an_empty_audit() {
    let v = audit(serde_json::json!({ "session_id": "sess-audit-none" })).await;
    assert_eq!(v["report"]["turns"].as_array().unwrap().len(), 0);
    assert_eq!(v["report"]["total_withheld"], 0);
    assert_eq!(v["truncated"], false);
}

/// Hitting the scan cap must be reported, not hidden: a truncated audit whose
/// totals look complete would misinform the operator it exists to inform.
#[tokio::test]
async fn truncation_is_reported_when_the_scan_cap_is_hit() {
    let sid = "sess-audit-trunc";
    for seq in 1..=3 {
        seed(
            sid,
            Some("t1"),
            seq,
            "egress.request_filtered",
            serde_json::json!({ "withheld_count": 1, "included_count": 0, "violation_count": 0 }),
        );
    }

    // limit below the seeded event count → the scan fills its window.
    let v = audit(serde_json::json!({ "session_id": sid, "limit": 2 })).await;
    assert_eq!(v["truncated"], true, "a filled window must be flagged");
    assert_eq!(v["limit"], 2);

    // A limit above the count is not truncated.
    let v = audit(serde_json::json!({ "session_id": sid, "limit": 500 })).await;
    assert_eq!(v["truncated"], false);
}

/// `limit` is a direct handle on how much this endpoint allocates
/// (`search_causal_events` materializes full rows, payloads included), so a
/// remote caller must not be able to widen it. It is clamped to
/// `1..=MAX_AUDIT_LIMIT` and the applied value echoed back, so a clamped
/// request is visible rather than silent. (PR #992 review.)
#[tokio::test]
async fn oversized_limit_is_clamped_to_the_ceiling() {
    let sid = "sess-audit-clamp";
    seed(
        sid,
        Some("t1"),
        1,
        "egress.request_filtered",
        serde_json::json!({ "withheld_count": 0, "included_count": 1, "violation_count": 0 }),
    );

    let v = audit(serde_json::json!({ "session_id": sid, "limit": 900_000_000_i64 })).await;
    assert_eq!(
        v["limit"],
        autonoetic_gateway::egress_audit::MAX_AUDIT_LIMIT,
        "a limit above the ceiling must be clamped, and the applied value echoed"
    );
    assert_eq!(v["truncated"], false);

    // Narrowing still works — the clamp is a ceiling, not a fixed value.
    let v = audit(serde_json::json!({ "session_id": sid, "limit": 1 })).await;
    assert_eq!(v["limit"], 1);

    // Nonsense lower bounds land on 1 rather than reaching the store as <= 0.
    for bad in [0_i64, -5] {
        let v = audit(serde_json::json!({ "session_id": sid, "limit": bad })).await;
        assert_eq!(v["limit"], 1, "limit {bad} must clamp to 1");
    }
}

#[tokio::test]
async fn empty_session_id_is_an_invalid_params_error() {
    let resp = rpc("egress.audit", serde_json::json!({ "session_id": "   " })).await;
    let err = resp.error.expect("expected an error");
    assert_eq!(err.code, -32602);
    assert!(
        err.message.contains("session_id"),
        "message should name the offending param: {}",
        err.message
    );
}

#[tokio::test]
async fn missing_session_id_is_an_invalid_params_error() {
    let resp = rpc("egress.audit", serde_json::json!({})).await;
    let err = resp.error.expect("expected an error");
    assert_eq!(err.code, -32602);
}

/// The report is safe to return over the wire only because it is content-free.
/// A payload key that carries content must never survive into the response —
/// this pins that, including for the noise event seeded alongside.
#[tokio::test]
async fn audit_response_carries_no_content_keys() {
    let sid = "sess-audit-metaonly";
    seed(
        sid,
        Some("t1"),
        1,
        "egress.envelope_withheld",
        serde_json::json!({
            "tool_call_id": "tc_x",
            "target_sink": "remote_model",
            "indication": "[withheld: 1× email_read result — policy local_only]",
            // A malformed emitter putting content in the payload must not leak
            // through the audit: the builder projects named fields only.
            "content": "CANARY-EMAIL-BODY",
            "stdout": "CANARY-STDOUT",
        }),
    );
    seed_noise(sid, 2);

    let v = audit(serde_json::json!({ "session_id": sid })).await;
    let wire = serde_json::to_string(&v).expect("serialize");

    for canary in ["CANARY-EMAIL-BODY", "CANARY-STDOUT", "SECRET-CONTENT"] {
        assert!(
            !wire.contains(canary),
            "audit response leaked content ({canary}): {wire}"
        );
    }
    for key in ["\"content\"", "\"stdout\"", "\"stderr\"", "\"message\""] {
        assert!(
            !wire.contains(key),
            "audit response carries a content key ({key}): {wire}"
        );
    }
    // …while the indication (metadata by construction, RFC §3.3) is present,
    // because that is what tells the operator what was withheld.
    assert!(wire.contains("policy local_only"));
}
