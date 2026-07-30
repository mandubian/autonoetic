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

use autonoetic_gateway::runtime::egress_labeler::{
    EgressLabeler, LabelRequest, PriorLabeledResult,
};
use autonoetic_gateway::runtime::egress_path_matcher::ExecSourceContext;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressConfig, EgressRule, EgressSessionPolicy, NamedEgressLabel};

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

fn no_prior() -> std::collections::HashMap<String, PriorLabeledResult> {
    std::collections::HashMap::new()
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
            Some(&store), &no_prior()
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
            Some(&store), &no_prior()
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
        Some(&store), &no_prior()
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
        Some(&store), &no_prior()
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
            Some(&store), &no_prior()
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
        Some(&store), &no_prior()
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
        Some(&store), &no_prior()
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
        Some(&store), &no_prior()
    );
    assert!(outcome.is_none());
    assert!(egress_events(&store, "sess-inert").is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Session-scoped policy (RFC §5.4) — declared, honored, isolated, and dying
// with the root session.
// ---------------------------------------------------------------------------

/// A policy declared on the store is what the labeler enforces, and the audit
/// event says the restriction came from the *session*, not standing config.
#[test]
fn stored_session_policy_labels_and_is_attributed_as_session_scoped() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let policy = EgressSessionPolicy {
        rules: vec![rule("email.*", None, NamedEgressLabel::LocalOnly)],
        default_label: None,
        provider_constraint: None,
    };
    store.set_egress_session_policy("sess-scoped", &policy, "operator:cli")?;

    let stored = store
        .get_egress_session_policy("sess-scoped")?
        .expect("policy round-trips");
    assert_eq!(stored.policy, policy);
    assert_eq!(stored.set_by, "operator:cli");

    // Global config declares nothing; the whole restriction is session-scoped.
    let labeler =
        EgressLabeler::from_config(&EgressConfig::default()).with_session_policy(&stored.policy);
    assert!(!labeler.is_inert(), "a session rule cancels the inert fast path");

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "email_read",
                arguments_json: r#"{"box":"inbox"}"#,
                tool_call_id: "tc_sess_1",
            },
            None,
            "sess-scoped",
            "researcher.default",
            Some("turn-000001"),
            Some(&store), &no_prior()
        )
        .expect("session rule labels the result");
    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());

    let events = egress_events(&store, "sess-scoped");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["resolution"], "session_rule");
    assert_eq!(payload["matched_rule_scopes"][0]["scope"], "session");
    assert_eq!(payload["matched_rule_scopes"][0]["rule"], "email.*");
    Ok(())
}

/// Two root sessions, one policy: the other session's labeler must not see it.
#[test]
fn session_policies_are_isolated_per_root_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    store.set_egress_session_policy(
        "sess-private",
        &EgressSessionPolicy {
            rules: vec![rule("slack.*", None, NamedEgressLabel::NoRemoteModel)],
            default_label: None,
            provider_constraint: None,
        },
        "operator:cli",
    )?;

    assert!(store.get_egress_session_policy("sess-private")?.is_some());
    assert!(
        store.get_egress_session_policy("sess-other")?.is_none(),
        "a policy declared on one root session must not be visible from another"
    );

    // And a labeler built for the other session labels nothing.
    let labeler = EgressLabeler::from_config(&EgressConfig::default());
    assert!(labeler.is_inert());
    Ok(())
}

/// The policy dies with the root session — the deletion the close path and
/// emergency stop both call.
#[test]
fn session_policy_is_deleted_with_the_root_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    store.set_egress_session_policy(
        "sess-ending",
        &EgressSessionPolicy {
            rules: vec![rule("email.*", None, NamedEgressLabel::LocalOnly)],
            default_label: None,
            provider_constraint: None,
        },
        "operator:rpc",
    )?;
    assert!(store.get_egress_session_policy("sess-ending")?.is_some());

    assert!(store.delete_egress_session_policy("sess-ending")?);
    assert!(store.get_egress_session_policy("sess-ending")?.is_none());
    // Deleting again is a no-op, not an error — close and emergency stop can
    // both run for the same session.
    assert!(!store.delete_egress_session_policy("sess-ending")?);
    Ok(())
}

/// `set` replaces rather than accumulates: the operator sees one policy
/// document per session.
#[test]
fn setting_a_policy_twice_replaces_it() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    store.set_egress_session_policy(
        "sess-replace",
        &EgressSessionPolicy {
            rules: vec![rule("email.*", None, NamedEgressLabel::LocalOnly)],
            default_label: None,
            provider_constraint: None,
        },
        "operator:cli",
    )?;
    let second = EgressSessionPolicy {
        rules: vec![rule("slack.*", None, NamedEgressLabel::NoRemoteModel)],
        default_label: Some(NamedEgressLabel::NoRemoteModel),
        provider_constraint: None,
    };
    store.set_egress_session_policy("sess-replace", &second, "operator:rpc")?;

    let stored = store.get_egress_session_policy("sess-replace")?.unwrap();
    assert_eq!(stored.policy, second);
    assert_eq!(stored.set_by, "operator:rpc");
    Ok(())
}

/// Global and session rules intersect — the session can tighten what standing
/// policy already restricted, never loosen it.
#[test]
fn session_rules_intersect_with_global_rules() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let labeler = EgressLabeler::from_config(&config_with(vec![rule(
        "email.*",
        None,
        NamedEgressLabel::NoRemoteModel,
    )]))
    .with_session_policy(&EgressSessionPolicy {
        rules: vec![rule("email.read", None, NamedEgressLabel::LocalOnly)],
        default_label: None,
        provider_constraint: None,
    });

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "email_read",
                arguments_json: "{}",
                tool_call_id: "tc_both",
            },
            None,
            "sess-both",
            "researcher.default",
            None,
            Some(&store), &no_prior()
        )
        .expect("labeled");
    // no_remote_model ∩ local_only = local_only (the stricter).
    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());

    let events = egress_events(&store, "sess-both");
    let payload: serde_json::Value = serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["resolution"], "operator_and_session_rule");
    let scopes: Vec<&str> = payload["matched_rule_scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["scope"].as_str().unwrap())
        .collect();
    assert!(scopes.contains(&"global"));
    assert!(scopes.contains(&"session"));
    Ok(())
}

// ---------------------------------------------------------------------------
// Static path matcher: direct + dependency reads (acceptance criterion 2).
// ---------------------------------------------------------------------------

/// RFC §5.6 step 3, the harder half: the labeled path appears only inside the
/// script the command names. Scanning the command line alone would miss it.
#[test]
fn sandbox_exec_labels_a_dependency_script_read() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(
        agent_dir.join("parse_mail.py"),
        "import mailbox\nmb = mailbox.mbox(\"~/mail/archive.mbox\")\nfor m in mb: print(m['Subject'])\n",
    )?;

    let labeler = EgressLabeler::from_config(&config_with(vec![rule(
        "sandbox.exec",
        Some("~/mail/**"),
        NamedEgressLabel::LocalOnly,
    )]));
    let req = LabelRequest {
        tool: "sandbox_exec",
        arguments_json: r#"{"command":"python3 parse_mail.py"}"#,
        tool_call_id: "tc_dep",
    };

    let ctx = ExecSourceContext {
        agent_dir: Some(&agent_dir),
        gateway_dir: Some(tmp.path()),
        session_id: Some("sess-dep"),
        gateway_store: None,
    };
    let outcome = labeler
        .label_tool_result(
            &req,
            Some(&ctx),
            "sess-dep",
            "coder.default",
            Some("turn-000003"),
            Some(&store), &no_prior()
        )
        .expect("the script's read of ~/mail/** must label the exec envelope");
    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());

    let events = egress_events(&store, "sess-dep");
    assert_eq!(events.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["matched_paths"][0], "~/mail/**");
    Ok(())
}

/// The direct read still works, and the audit records the default in force so a
/// label produced by the default alone is explainable too.
#[test]
fn audit_event_carries_the_default_in_force() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let mut cfg = EgressConfig::default();
    cfg.default_label = NamedEgressLabel::NoRemoteModel;
    let labeler = EgressLabeler::from_config(&cfg);

    let outcome = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "web_search",
                arguments_json: r#"{"q":"anything"}"#,
                tool_call_id: "tc_default",
            },
            None,
            "sess-default",
            "researcher.default",
            None,
            Some(&store), &no_prior()
        )
        .expect("a restricting default labels everything");
    assert_eq!(
        outcome.label,
        autonoetic_types::egress::EgressLabel::no_remote_model()
    );

    let events = egress_events(&store, "sess-default");
    let payload: serde_json::Value = serde_json::from_str(events[0].payload.as_ref().unwrap())?;
    assert_eq!(payload["resolution"], "default");
    assert_eq!(payload["session_default_applied"], false);
    assert!(payload["matched_rules"].as_array().unwrap().is_empty());
    let default_sinks: Vec<&str> = payload["default_label"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(default_sinks.contains(&"local_model"));
    assert!(!default_sinks.contains(&"remote_model"));
    Ok(())
}

/// A rule written in the documented dotted form must match the runtime's
/// canonical snake_case tool name. Before normalization every rule copied from
/// the RFC or the config template was a silent no-op.
#[test]
fn documented_dotted_rules_match_canonical_tool_names() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let labeler = EgressLabeler::from_config(&config_with(vec![
        rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        rule("mcp.gmail.*", None, NamedEgressLabel::LocalOnly),
    ]));

    let exec = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
                tool_call_id: "tc_norm_exec",
            },
            None,
            "sess-norm",
            "coder.default",
            None,
            Some(&store), &no_prior()
        )
        .expect("`sandbox.exec` rule must match the `sandbox_exec` tool");
    assert_eq!(exec.label, autonoetic_types::egress::EgressLabel::local_only());

    let mcp = labeler
        .label_tool_result(
            &LabelRequest {
                tool: "mcp_gmail_send_message",
                arguments_json: "{}",
                tool_call_id: "tc_norm_mcp",
            },
            None,
            "sess-norm",
            "coder.default",
            None,
            Some(&store), &no_prior()
        )
        .expect("`mcp.gmail.*` rule must match `mcp_gmail_send_message`");
    assert_eq!(mcp.label, autonoetic_types::egress::EgressLabel::local_only());

    assert_eq!(egress_events(&store, "sess-norm").len(), 2);
    Ok(())
}

/// `artifact_ref` — the short `ar.*` form the `sandbox_exec` schema tells agents
/// to *prefer* — is not a bundle id: `ArtifactStore::inspect` asserts the `art_`
/// prefix. Without resolving it through the ref registry, a ref-driven exec has
/// no bundle to scan and a path-bearing rule silently fails to label.
/// (PR #914 review.)
#[test]
fn artifact_ref_driven_exec_resolves_its_bundle_and_labels() -> anyhow::Result<()> {
    use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};

    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // A real bundle whose script reads a labeled path.
    let artifact_store = autonoetic_gateway::artifact_store::ArtifactStore::new(tmp.path())?;
    let content = autonoetic_gateway::runtime::content_store::ContentStore::new(tmp.path())?;
    let handle = content.write(b"import mailbox\nmailbox.mbox('~/mail/archive.mbox')\n")?;
    content.register_name("sess-ref", "parse_mail.py", &handle)?;
    let bundle = artifact_store.build(&["parse_mail.py".into()], None, None, "sess-ref")?;

    // …reachable only by its short ref.
    store.create_artifact_ref(&ArtifactRefRecord {
        ref_id: "ar.mailparse01".to_string(),
        scope_type: ArtifactRefScopeType::Session,
        scope_id: "sess-ref".to_string(),
        artifact_id: bundle.artifact_id.clone(),
        artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
        artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
        created_by_agent_id: "coder.default".to_string(),
        created_at: "2026-07-27T00:00:00Z".to_string(),
        expires_at: None,
        revoked_at: None,
    })?;

    let labeler = EgressLabeler::from_config(&config_with(vec![rule(
        "sandbox.exec",
        Some("~/mail/**"),
        NamedEgressLabel::LocalOnly,
    )]));
    let req = LabelRequest {
        tool: "sandbox_exec",
        // The command names nothing labeled; only the bundle's script does.
        arguments_json: r#"{"artifact_ref":"ar.mailparse01","command":"python3 parse_mail.py"}"#,
        tool_call_id: "tc_ref",
    };
    let ctx = ExecSourceContext {
        agent_dir: None,
        gateway_dir: Some(tmp.path()),
        session_id: Some("sess-ref"),
        gateway_store: Some(&store),
    };

    let outcome = labeler
        .label_tool_result(&req, Some(&ctx), "sess-ref", "coder.default", None, Some(&store), &no_prior())
        .expect("a ref-driven exec must have its bundle scanned");
    assert_eq!(outcome.label, autonoetic_types::egress::EgressLabel::local_only());

    // Without the store there is nothing to resolve the ref with — the bundle
    // is invisible and the result goes unlabeled. This is the bug the fix
    // closes, pinned so the wiring cannot regress silently.
    let no_store_ctx = ExecSourceContext {
        agent_dir: None,
        gateway_dir: Some(tmp.path()),
        session_id: Some("sess-ref"),
        gateway_store: None,
    };
    assert!(
        labeler
            .label_tool_result(&req, Some(&no_store_ctx), "sess-ref", "coder.default", None, None, &no_prior())
            .is_none(),
        "an unresolved artifact_ref leaves nothing to scan"
    );
    Ok(())
}
