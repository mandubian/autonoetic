//! Integration: egress source rules → `egress.envelope_labeled` causal events.
//!
//! RFC data-envelopes §4.2 / §9.1. Phase 1c (#906) acceptance:
//! - operator source rules label tool results at the commit boundary,
//! - `egress.envelope_labeled` is persisted with correct provenance so
//!   "why is this envelope labeled?" is answerable from the causal chain,
//! - sandbox.exec reading a labeled path produces a labeled result envelope,
//! - a path-bearing rule for one source does NOT label a different source's
//!   result (PR #911 review regression),
//! - session-scoped rules do not leak into another session's labeler.
//!
//! The test drives [`EgressLabeler`] directly against a real [`GatewayStore`]
//! (tempfile-isolated). It does not spin up a full session: the labeler is the
//! unit under test, and the causal-event persistence is the integration
//! boundary being verified. A full session-level canary test lands in phase 1b
//! (#905) once the chokepoint exists.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{EgressLabeler, LabelRequest};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressConfig, EgressRule, NamedEgressLabel};

fn rule(source: &str, path: Option<&str>, label: NamedEgressLabel) -> EgressRule {
    EgressRule {
        source: source.to_string(),
        path: path.map(|s| s.to_string()),
        label: label.to_label(),
    }
}

fn config_with(rules: Vec<EgressRule>) -> EgressConfig {
    EgressConfig {
        rules,
        ..Default::default()
    }
}

/// Read back the `egress.envelope_labeled` events for a session, filtered by
/// action (the store API filters on session/agent, not action).
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

#[test]
fn labeled_tool_result_emits_causal_event_with_provenance() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let cfg = config_with(vec![rule("email.read", None, NamedEgressLabel::LocalOnly)]);
    let labeler = EgressLabeler::from_config(&cfg);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "email.read",
                arguments_json: r#"{"box":"inbox","n":3}"#,
                tool_call_id: "tc_email_1",
            },
            None,
            "sess-email",
            "researcher.default",
            Some("turn-000001"),
            Some(&store),
        )
        .expect("email.read should be labeled local_only");

    // The label is local_only (the rule's label).
    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());
    assert!(outcome.envelope_id.starts_with("env_"));
    assert_eq!(outcome.provenance.tool.as_deref(), Some("email.read"));
    assert_eq!(outcome.provenance.matched_rules, vec!["email.read"]);

    // The causal event is persisted with content-free metadata.
    let events = egress_events(&store, "sess-email");
    assert_eq!(events.len(), 1, "exactly one egress.envelope_labeled event");
    let ev = &events[0];
    assert_eq!(ev.category, "egress");
    assert_eq!(ev.action, "egress.envelope_labeled");
    assert_eq!(ev.session_id, "sess-email");
    assert_eq!(ev.agent_id, "researcher.default");
    assert_eq!(ev.turn_id.as_deref(), Some("turn-000001"));
    assert_eq!(ev.target.as_deref(), Some(outcome.envelope_id.as_str()));

    let payload: serde_json::Value = serde_json::from_str(ev.payload.as_ref().unwrap())?;
    assert_eq!(payload["tool_name"], "email.read");
    assert_eq!(payload["tool_call_id"], "tc_email_1");
    assert_eq!(payload["resolution"], "operator_rule");
    assert_eq!(payload["matched_rules"][0], "email.read");
    // The label serializes as its snake_case sink set.
    let label_arr = payload["label"].as_array().expect("label is array");
    let sinks: Vec<&str> = label_arr.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(sinks.contains(&"local_model"));
    assert!(sinks.contains(&"memory_persist"));
    assert!(!sinks.contains(&"remote_model"));
    // args_digest is present (12 hex chars) but never the arguments content.
    let digest = payload["args_digest"].as_str().unwrap();
    assert_eq!(digest.len(), 12);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    Ok(())
}

#[test]
fn sandbox_exec_reading_labeled_path_produces_labeled_envelope() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let cfg = config_with(vec![
        rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
    ]);
    let labeler = EgressLabeler::from_config(&cfg);

    // RFC §5.6 step 3: the script (not a structured tool) reads ~/mail/**.
    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox.exec",
                arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
                tool_call_id: "tc_exec_1",
            },
            None,
            "sess-exec",
            "coder.default",
            Some("turn-000002"),
            Some(&store),
        )
        .expect("sandbox.exec touching ~/mail/** should be labeled");

    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());
    // Provenance records the path-scoped rule that fired.
    assert!(outcome
        .provenance
        .matched_rules
        .iter()
        .any(|r| r.contains("~/mail/**")));

    let events = egress_events(&store, "sess-exec");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["tool_name"], "sandbox.exec");
    assert_eq!(payload["resolution"], "operator_rule");
    Ok(())
}

/// Regression for the source-mismatch bug (PR #911 review): a path-bearing
/// rule for `fs.read` must NOT label a `sandbox.exec` result even when the
/// command touches the same path. Only rules whose `source` matches the tool
/// being labeled may apply.
#[test]
fn sandbox_exec_ignores_path_rules_for_other_sources() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let cfg = config_with(vec![
        // fs.read rule with the same path pattern — must not bleed into sandbox.exec.
        rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
    ]);
    let labeler = EgressLabeler::from_config(&cfg);

    let outcome = labeler.label_tool_result(
        &LabelRequest {
            tool: "sandbox.exec",
            arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
            tool_call_id: "tc_exec_cross",
        },
        None,
        "sess-cross",
        "coder.default",
        None,
        Some(&store),
    );
    assert!(
        outcome.is_none(),
        "fs.read path rule must not label a sandbox.exec result"
    );
    assert!(egress_events(&store, "sess-cross").is_empty());
    Ok(())
}

#[test]
fn clean_tool_result_emits_no_event() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // Rule only labels email.*; a web_search call stays unrestricted.
    let cfg = config_with(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]);
    let labeler = EgressLabeler::from_config(&cfg);

    let outcome = labeler.label_tool_result(
        &LabelRequest {
            tool: "web_search",
            arguments_json: r#"{"q":"rust async"}"#,
            tool_call_id: "tc_web_1",
        },
        None,
        "sess-clean",
        "researcher.default",
        Some("turn-000003"),
        Some(&store),
    );
    assert!(outcome.is_none(), "clean result must not be labeled");
    assert!(egress_events(&store, "sess-clean").is_empty());
    Ok(())
}

#[test]
fn multiple_matching_rules_intersect_in_the_event_label() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // Two rules both match fs.read on ~/mail/** — local_only and no_remote_model.
    // Intersection = local_only (the stricter). Both rules appear in provenance.
    let cfg = config_with(vec![
        rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        rule("fs.read", Some("~/mail/**"), NamedEgressLabel::NoRemoteModel),
    ]);
    let labeler = EgressLabeler::from_config(&cfg);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "fs.read",
                arguments_json: r#"{"path":"~/mail/inbox/2"}"#,
                tool_call_id: "tc_fs_1",
            },
            None,
            "sess-intersect",
            "researcher.default",
            Some("turn-000004"),
            Some(&store),
        )
        .expect("fs.read on ~/mail/** is labeled");

    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());
    assert_eq!(outcome.provenance.matched_rules.len(), 2);

    let events = egress_events(&store, "sess-intersect");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    let sinks: Vec<&str> = payload["label"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(sinks.contains(&"local_model"));
    assert!(!sinks.contains(&"remote_model"));
    assert!(!sinks.contains(&"network")); // local_only excludes Network
    Ok(())
}

#[test]
fn session_scoped_rules_do_not_leak_across_sessions() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // Session A has a session-scoped rule for slack.*.
    let labeler_a = EgressLabeler::from_config(&config_with(vec![]))
        .with_session_rules(vec![rule("slack.*", None, NamedEgressLabel::NoRemoteModel)]);
    // Session B is a fresh labeler with no session rules.
    let labeler_b = EgressLabeler::from_config(&config_with(vec![]));

    // Session A labels slack.read.
    let out_a = labeler_a.label_tool_result(
        &LabelRequest {
            tool: "slack.read",
            arguments_json: "{}",
            tool_call_id: "tc_a",
        },
        None,
        "sess-a",
        "agent-a",
        None,
        Some(&store),
    );
    assert!(out_a.is_some(), "session A's rule labels slack.read");

    // Session B does NOT — its labeler has no slack rule.
    let out_b = labeler_b.label_tool_result(
        &LabelRequest {
            tool: "slack.read",
            arguments_json: "{}",
            tool_call_id: "tc_b",
        },
        None,
        "sess-b",
        "agent-b",
        None,
        Some(&store),
    );
    assert!(out_b.is_none(), "session-scoped rules must not leak into session B");

    // And the event only lands in session A's causal chain.
    assert_eq!(egress_events(&store, "sess-a").len(), 1);
    assert!(egress_events(&store, "sess-b").is_empty());
    Ok(())
}

#[test]
fn unconfigured_deployment_is_inert_and_emits_nothing() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // Default config: no rules, default_label unrestricted → inert.
    let labeler = EgressLabeler::from_config(&EgressConfig::default());
    assert!(labeler.is_inert());

    let outcome = labeler.label_tool_result(
        &LabelRequest {
            tool: "fs.read",
            arguments_json: r#"{"path":"~/mail/anything"}"#,
            tool_call_id: "tc_inert",
        },
        None,
        "sess-inert",
        "agent",
        None,
        Some(&store),
    );
    assert!(outcome.is_none());
    assert!(egress_events(&store, "sess-inert").is_empty());
    Ok(())
}
