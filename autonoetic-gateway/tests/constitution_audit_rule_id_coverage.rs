use autonoetic_gateway::runtime::session_tracer::SessionTracer;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

#[test]
fn causal_decision_events_include_enforced_rule_ids() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let agent_dir = tempdir.path().join("agents").join("tester.default");
    std::fs::create_dir_all(&agent_dir)?;

    let mut tracer = SessionTracer::new_with_evidence_mode(&agent_dir, "tester.default", "sess-rule-1", "off")?
        .with_gateway_store(Some(store.clone()))
        .with_turn_id("turn-1");

    tracer.log_event(
        "policy",
        "tool.accepted",
        autonoetic_types::causal_chain::EntryStatus::Success,
        Some(serde_json::json!({"tool_name": "content_write"})),
    )?;

    tracer.log_event(
        "policy",
        "tool.rejected",
        autonoetic_types::causal_chain::EntryStatus::Denied,
        Some(serde_json::json!({
            "tool_name": "sandbox_exec",
            "reason": "missing capability",
            "enforced_rules": ["R-1.1", "R-1.10"]
        })),
    )?;

    tracer.log_event(
        "approval",
        "gate.pending",
        autonoetic_types::causal_chain::EntryStatus::Success,
        Some(serde_json::json!({"approval_required": true})),
    )?;

    let events = store.search_causal_events(Some("sess-rule-1"), Some("tester.default"), 50)?;
    assert!(events.len() >= 3, "expected at least 3 events, got {}", events.len());

    for event in &events {
        assert!(
            !event.enforced_rules.is_empty(),
            "event {} missing enforced_rules",
            event.event_id
        );
    }

    let rejected = events
        .iter()
        .find(|event| event.action == "tool.rejected")
        .expect("expected tool.rejected event");
    assert_eq!(rejected.enforced_rules, vec!["R-1.1", "R-1.10"]);

    Ok(())
}
