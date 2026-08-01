//! Envelope → artifact forward link (RFC §4.5, #986): the artifact half of the
//! taint-provenance question. A labeled `artifact_build` result becomes an
//! artifact; its envelope's provenance must name the artifact_id, and the
//! `egress.envelope_labeled` payload must surface it — so an operator following
//! a tainted envelope can land on the durable bytes it left behind.
//!
//! The reverse direction (artifact labels read *into* a resolution) is covered
//! by `stored_content.rs` / `artifact_labels_applied`.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_gateway::runtime::egress_labeler::{EgressLabeler, LabelRequest};
use autonoetic_types::egress::{EgressConfig, EgressLabel, EgressRule};
use std::collections::HashMap;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("artifact-provenance-test", method, params).await
}

fn labeler_restricting_builds() -> EgressLabeler {
    EgressLabeler::from_config(&EgressConfig {
        rules: vec![EgressRule {
            source: "artifact.build".to_string(),
            path: None,
            label: EgressLabel::local_only(),
        }],
        ..Default::default()
    })
}

fn labeler_restricting_inspects() -> EgressLabeler {
    EgressLabeler::from_config(&EgressConfig {
        rules: vec![EgressRule {
            source: "artifact.inspect".to_string(),
            path: None,
            label: EgressLabel::local_only(),
        }],
        ..Default::default()
    })
}

/// The full walk: a labeled artifact_build result carries the produced
/// artifact id on its envelope, and `egress.audit` surfaces it in the
/// `egress.envelope_labeled` payload.
#[tokio::test]
async fn produced_artifact_id_lands_in_the_envelope_labeled_payload() {
    let e = env();
    let agent = "coder.artprov";
    let session = "artprov-root/coder.artprov-1";

    let out = labeler_restricting_builds()
        .label_tool_result(
            &LabelRequest {
                tool: "artifact_build",
                arguments_json: r#"{"inputs":["main.py"],"entrypoints":["main.py"]}"#,
                tool_call_id: "tc_artprov",
                artifact_id: Some("art_deadbeef"),
            },
            None,
            session,
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("artifact_build under a local_only rule must label");
    assert_eq!(out.label, EgressLabel::local_only());
    assert_eq!(
        out.provenance.artifact_id.as_deref(),
        Some("art_deadbeef"),
        "the envelope must map to the artifact its content became"
    );

    // The audit row carries the same link (RFC §9.3 — the report is
    // content-free metadata, and an artifact id is a store key).
    let resp = rpc(
        "egress.audit",
        serde_json::json!({ "session_id": session }),
    )
    .await;
    assert!(resp.error.is_none(), "egress.audit error: {:?}", resp.error);
    let report = resp.result.unwrap();
    let turns = report["report"]["turns"].as_array().expect("turns");
    let rows = turns[0]["rows"].as_array().expect("rows");
    let labeled = rows
        .iter()
        .find(|r| r["action"] == "egress.envelope_labeled")
        .expect("the envelope_labeled row must exist");
    assert_eq!(
        labeled["fields"]["artifact_id"],
        serde_json::json!("art_deadbeef"),
        "the audit must surface the produced artifact"
    );
    assert_eq!(
        labeled["fields"]["tool_name"],
        serde_json::json!("artifact_build")
    );
}

/// A produced artifact id never leaks onto other tools' envelopes: only
/// `artifact_build` writes into the artifact store, and the chokepoint gates
/// the parse on that tool.
#[tokio::test]
async fn other_tools_never_carry_a_produced_artifact_id() {
    let e = env();
    let agent = "coder.artprov2";
    let session = "artprov-root2/coder.artprov2-1";

    let out = labeler_restricting_inspects()
        .label_tool_result(
            &LabelRequest {
                tool: "artifact_inspect",
                arguments_json: r#"{"artifact_ref":"ar.test12345678"}"#,
                tool_call_id: "tc_artprov2",
                artifact_id: None,
            },
            None,
            session,
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("artifact_inspect under a local_only artifact.inspect rule must label");
    assert_eq!(out.label, EgressLabel::local_only());
    assert_eq!(
        out.provenance.artifact_id, None,
        "inspecting an artifact is not producing one"
    );
}
