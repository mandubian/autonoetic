//! Phase 4 (#909) slice 4: OFP AgentMessage egress labels + inbound fail-closed taint.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{
    ofp_federated_egress_refusal, ofp_inbound_fail_closed_label, ofp_outbound_allows_federation,
    ofp_outbound_wire_fields, parse_ofp_inbound_egress_label,
};
use autonoetic_types::egress::{EgressLabel, Sink};

#[test]
fn ofp_inbound_missing_label_is_fail_closed_not_unrestricted() {
    let label = parse_ofp_inbound_egress_label(None);
    assert_eq!(label, ofp_inbound_fail_closed_label());
    assert!(!label.is_unrestricted());
    assert!(!label.allows(Sink::FederatedAgent));
    assert!(!label.allows(Sink::RemoteModel));
}

#[test]
fn ofp_inbound_malformed_label_is_fail_closed() {
    let bad = serde_json::json!("not-a-label");
    let label = parse_ofp_inbound_egress_label(Some(&bad));
    assert_eq!(label, ofp_inbound_fail_closed_label());
}

#[test]
fn ofp_inbound_parses_valid_label() {
    let raw = serde_json::to_value(EgressLabel::local_only()).unwrap();
    let label = parse_ofp_inbound_egress_label(Some(&raw));
    assert_eq!(label, EgressLabel::local_only());
}

#[test]
fn ofp_outbound_refuses_local_only_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        tmp.path(),
    )?);
    let session_id = "root-ofp/planner";
    store.set_session_egress_taint(session_id, &EgressLabel::local_only())?;

    let err = ofp_federated_egress_refusal(
        "secret payload",
        Some(session_id),
        "planner.default",
        Some(&store),
    )
    .expect("expected OFP federation refusal for local_only session");

    assert!(
        err.to_string().contains("FederatedAgent"),
        "refusal should name the blocked sink: {err}"
    );

    let events = store.search_causal_events(Some(session_id), None, 10)?;
    assert!(
        events.iter().any(|e| e.action == "egress.boundary_refused"),
        "expected egress.boundary_refused audit event"
    );
    Ok(())
}

#[test]
fn ofp_outbound_wire_fields_omit_unrestricted_label() {
    let (msg, label, withheld) = ofp_outbound_wire_fields("hi", &EgressLabel::unrestricted());
    assert_eq!(msg, "hi");
    assert!(label.is_none());
    assert!(withheld.is_none());
    assert!(ofp_outbound_allows_federation(&EgressLabel::unrestricted()));
}

#[test]
fn ofp_outbound_wire_fields_include_restrictive_label() {
    let taint = EgressLabel::local_only();
    let (msg, label, withheld) = ofp_outbound_wire_fields("hi", &taint);
    assert_eq!(msg, "hi");
    assert!(withheld.is_none());
    let parsed: EgressLabel = serde_json::from_value(label.expect("label on wire")).unwrap();
    assert_eq!(parsed, taint);
    assert!(!ofp_outbound_allows_federation(&taint));
}
