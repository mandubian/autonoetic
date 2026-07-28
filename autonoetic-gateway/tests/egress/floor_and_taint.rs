//! Integration: bundle-declared floor + argument taint (RFC §4.1 paths 2 + 3).
//!
//! #907 slice 1. Extends the phase-1c egress source-rules suite with:
//! - bundle-declared output floor (`metadata.autonoetic.egress.output_label`)
//!   restricts every result, clears inertness, intersects with operator rules;
//! - argument taint — a tool called with a reference to (or verbatim content
//!   from) a prior labeled result inherits that label;
//! - the `egress.envelope_labeled` event records both new resolution inputs
//!   (`bundle_floor_applied`, `parent_envelope_ids`, `taint_applied`).
//!
//! The tests drive [`EgressLabeler`] directly against a real [`GatewayStore`]
//! (tempfile-isolated), matching the harness in `egress_source_rules_integration`.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{
    EgressLabeler, LabelRequest, PriorLabeledResult,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressConfig, EgressLabel, EgressRule, NamedEgressLabel};

fn rule(source: &str, label: NamedEgressLabel) -> EgressRule {
    EgressRule {
        source: source.to_string(),
        path: None,
        label: label.to_label(),
    }
}

fn config_with(rules: Vec<EgressRule>) -> EgressConfig {
    EgressConfig {
        rules,
        ..Default::default()
    }
}

fn no_prior() -> std::collections::HashMap<String, PriorLabeledResult> {
    std::collections::HashMap::new()
}

fn prior(
    entries: &[(&str, EgressLabel, Option<&str>)],
) -> std::collections::HashMap<String, PriorLabeledResult> {
    entries
        .iter()
        .map(|(tcid, label, snip)| {
            (
                tcid.to_string(),
                PriorLabeledResult {
                    label: label.clone(),
                    content_snippet: snip.map(|s| s.to_string()),
                },
            )
        })
        .collect()
}

fn egress_events(
    store: &GatewayStore,
    session_id: &str,
) -> Vec<autonoetic_types::causal_chain::CausalEventRecord> {
    store
        .search_causal_events(Some(session_id), None, 50)
        .expect("search_causal_events")
        .into_iter()
        .filter(|e| e.action == "egress.envelope_labeled")
        .collect()
}

// ── Bundle floor (RFC §4.1 path 2) ─────────────────────────────────────

#[test]
fn floor_applies_with_no_operator_rules_and_emits_event() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // No operator rules at all, unrestricted default — but a bundle-only floor.
    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::local_only()));
    assert!(!labeler.is_inert());

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "custom_email_reader",
                arguments_json: r#"{"box":"inbox"}"#,
                tool_call_id: "tc_floor_1",
            },
            None,
            "sess-floor",
            "email-agent.default",
            Some("turn-1"),
            Some(&store),
            &no_prior(),
        )
        .expect("floor should produce a restricted envelope");

    assert_eq!(outcome.label, EgressLabel::local_only());

    // The event records the floor as a resolution input.
    let events = egress_events(&store, "sess-floor");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["bundle_floor_applied"], true);
    assert_eq!(payload["taint_applied"], false);
    assert_eq!(payload["parent_envelope_ids"].as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn floor_cannot_widen_operator_local_only() -> anyhow::Result<()> {
    // Operator restricts email.* to local_only; bundle declares unrestricted
    // floor → result stays local_only (floor can't widen).
    let labeler = EgressLabeler::from_config(&config_with(vec![rule(
        "email.read",
        NamedEgressLabel::LocalOnly,
    )]))
    .with_manifest_floor(Some(EgressLabel::unrestricted()));

    let r = labeler.resolve_label("email.read", None);
    assert_eq!(r.label, EgressLabel::local_only());
    Ok(())
}

#[test]
fn bundle_local_only_floor_intersects_operator_no_remote_model() -> anyhow::Result<()> {
    // Operator says no_remote_model; bundle floor is local_only → intersection
    // = local_only (the stricter).
    let labeler = EgressLabeler::from_config(&config_with(vec![rule(
        "fs.read",
        NamedEgressLabel::NoRemoteModel,
    )]))
    .with_manifest_floor(Some(EgressLabel::local_only()));

    let r = labeler.resolve_label("fs.read", None);
    assert_eq!(r.label, EgressLabel::local_only());
    Ok(())
}

#[test]
fn floor_persists_across_session_policy_merge() -> anyhow::Result<()> {
    // A session policy is merged AFTER the floor; the floor must survive the
    // merge (the builder chains from_config → with_manifest_floor →
    // with_session_policy).
    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::local_only()))
        .with_session_policy(&autonoetic_types::egress::EgressSessionPolicy {
            rules: vec![rule("slack.*", NamedEgressLabel::NoRemoteModel)],
            default_label: None,
        });

    // The floor still applies to unmentioned sources.
    let r = labeler.resolve_label("unknown_tool", None);
    assert_eq!(r.label, EgressLabel::local_only());
    assert!(r.bundle_floor_applied);

    // And the session rule applies on top.
    let r2 = labeler.resolve_label("slack.read", None);
    // local_only ∩ no_remote_model = local_only
    assert_eq!(r2.label, EgressLabel::local_only());
    Ok(())
}

// ── Argument taint (RFC §4.1 path 3) ───────────────────────────────────

#[test]
fn taint_by_handle_reference_labels_and_records_parents() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::unrestricted()));

    let taint = prior(&[("tc_email_secret", EgressLabel::local_only(), None)]);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "content_write",
                arguments_json: r#"{"source_ref":"tc_email_secret","content":"summary"}"#,
                tool_call_id: "tc_derived",
            },
            None,
            "sess-taint",
            "agent.default",
            Some("turn-1"),
            Some(&store),
            &taint,
        )
        .expect("tainted output should be labeled");

    assert_eq!(outcome.label, EgressLabel::local_only());
    assert_eq!(
        outcome.provenance.parent_envelope_ids,
        vec!["tc_email_secret".to_string()]
    );

    // The event records taint info.
    let events = egress_events(&store, "sess-taint");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["taint_applied"], true);
    assert_eq!(payload["parent_envelope_ids"][0], "tc_email_secret");
    Ok(())
}

#[test]
fn taint_by_verbatim_content_labels_output() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::unrestricted()));

    let canary = "CANARY-SECRET-CONTENT-FROM-EMAIL";
    let taint = prior(&[("tc_email", EgressLabel::local_only(), Some(canary))]);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "content_write",
                arguments_json: &format!(r#"{{"content":"Forwarding: {canary}"}}"#),
                tool_call_id: "tc_copy",
            },
            None,
            "sess-verbatim",
            "agent.default",
            None,
            Some(&store),
            &taint,
        )
        .expect("verbatim taint should fire");

    assert_eq!(outcome.label, EgressLabel::local_only());
    assert!(outcome
        .provenance
        .parent_envelope_ids
        .contains(&"tc_email".to_string()));
    Ok(())
}

#[test]
fn clean_argument_with_no_matching_taint_is_unrestricted() -> anyhow::Result<()> {
    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::unrestricted()));

    let taint = prior(&[(
        "tc_secret",
        EgressLabel::local_only(),
        Some("SECRET-NOT-PRESENT-IN-ARGS"),
    )]);

    let outcome = labeler.label_tool_result(
        &LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"content":"clean unrelated content"}"#,
            tool_call_id: "tc_clean",
        },
        None,
        "sess",
        "agent",
        None,
        None,
        &taint,
    );
    assert!(outcome.is_none(), "clean argument → unrestricted → no envelope");
    Ok(())
}

#[test]
fn taint_intersection_of_local_only_and_no_remote_model() -> anyhow::Result<()> {
    let labeler = EgressLabeler::from_config(&EgressConfig::default())
        .with_manifest_floor(Some(EgressLabel::unrestricted()));

    let taint = prior(&[
        ("tc_local", EgressLabel::local_only(), None),
        ("tc_conf", EgressLabel::no_remote_model(), None),
    ]);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "content_write",
                arguments_json: r#"{"refs":["tc_local","tc_conf"]}"#,
                tool_call_id: "tc_out",
            },
            None,
            "sess",
            "agent",
            None,
            None,
            &taint,
        )
        .expect("should be labeled by taint intersection");

    // local_only ∩ no_remote_model = local_only
    assert_eq!(outcome.label, EgressLabel::local_only());
    assert_eq!(outcome.provenance.parent_envelope_ids.len(), 2);
    Ok(())
}
