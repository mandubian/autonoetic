//! RFC §5.3 pin × taint inline ask (#968) — store-level acceptance.
//!
//! The full routing decision (file ask → suspend → honor the answer) is
//! covered by in-crate unit tests on `AgentExecutor::plan_egress_routing`
//! (lifecycle.rs `#968` block); this suite exercises the public store-level
//! surface end to end against a real `GatewayStore`: the ask's shape, dedup,
//! the answer-time declassification materialization, and the
//! `egress.provider_selected` inline-ask payload.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{
    apply_egress_ask_declassification, egress_ask_options, egress_ask_options_payload,
    emit_provider_selected, file_egress_pin_ask, is_egress_pin_ask,
    plan_taint_following_route, session_sink_declassified, PresetCandidate, EgressRoutingPlan,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{UserInteractionAnswer, UserInteractionKind, UserInteractionStatus};
use autonoetic_types::egress::{EgressClass, EgressLabel, Sink};

fn cand(name: &str, class: EgressClass) -> PresetCandidate {
    PresetCandidate {
        name: name.to_string(),
        egress_class: Some(class),
    }
}

fn store_in(tmp: &tempfile::TempDir) -> Arc<GatewayStore> {
    Arc::new(GatewayStore::open(tmp.path()).expect("open store"))
}

fn provider_selected_payloads(
    store: &GatewayStore,
) -> Vec<serde_json::Value> {
    store
        .search_causal_events(Some("sess-pin-ask"), None, 100)
        .expect("search_causal_events")
        .into_iter()
        .filter(|e| e.action == "egress.provider_selected")
        .map(|e| serde_json::from_str(e.payload.as_deref().unwrap_or("{}")).unwrap_or_default())
        .collect()
}

#[test]
fn pin_ask_is_a_decision_interaction_with_the_three_way_options() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);

    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000007",
        &EgressLabel::local_only(),
        "remote",
        Some(&local),
    )
    .expect("ask filed");

    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    assert!(is_egress_pin_ask(&it));
    assert_eq!(it.kind, UserInteractionKind::Decision);
    assert!(!it.allow_freeform);
    assert_eq!(it.status, UserInteractionStatus::Pending);
    assert_eq!(it.turn_id, "turn-000007");
    let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["declassify", "run_local", "abort"]);
    assert_eq!(
        egress_ask_options_payload(Some(&local)),
        vec!["declassify", "run_local", "abort"]
    );
    // The question names the pinned preset and the batch.
    assert!(it.question.contains("remote"));
    assert!(it.question.contains("local"));
}

#[test]
fn pin_ask_without_local_preset_omits_run_local() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000007",
        &EgressLabel::local_only(),
        "remote",
        None,
    )
    .expect("ask filed");
    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["declassify", "abort"]);
    assert_eq!(
        egress_ask_options_payload(None),
        vec!["declassify", "abort"]
    );
}

#[test]
fn pin_ask_dedups_while_pending() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);
    let a = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000007",
        &EgressLabel::local_only(),
        "remote",
        Some(&local),
    )
    .expect("first ask");
    let b = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000008",
        &EgressLabel::local_only(),
        "remote",
        Some(&local),
    )
    .expect("second ask");
    assert_eq!(a, b, "pending ask is reused, not re-filed");
    assert_eq!(
        store
            .get_pending_interactions_for_root_session("sess-pin-ask")
            .expect("pending")
            .len(),
        1
    );
}

#[test]
fn answered_declassify_materializes_session_wide_grant_and_emits_event() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000007",
        &EgressLabel::local_only(),
        "remote",
        Some(&local),
    )
    .expect("ask filed");

    store
        .answer_user_interaction(&UserInteractionAnswer {
            interaction_id: ask_id.clone(),
            answer_option_id: Some(egress_ask_options::DECLASSIFY.to_string()),
            answer_text: None,
            answered_by: "test-operator".to_string(),
        })
        .expect("answered");

    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    assert!(it.status == UserInteractionStatus::Answered);

    // The answer-time hook (interaction_answer.rs calls this right after
    // `answer_user_interaction`).
    apply_egress_ask_declassification(&store, &it).expect("grant materialized");

    assert!(
        session_sink_declassified(&store, "sess-pin-ask", "sess-pin-ask", Sink::RemoteModel),
        "the declassify answer widens the session to RemoteModel"
    );
    let events = store
        .search_causal_events(Some("sess-pin-ask"), None, 100)
        .expect("events");
    let declass = events
        .iter()
        .find(|e| e.action == "egress.declassified")
        .expect("egress.declassified emitted");
    let payload: serde_json::Value =
        serde_json::from_str(declass.payload.as_deref().unwrap_or("{}")).unwrap_or_default();
    assert_eq!(payload["allowed_sink"], "remote_model");
    assert_eq!(payload["scope"], "root_session");
}

#[test]
fn answered_run_local_does_not_materialize_anything() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000007",
        &EgressLabel::local_only(),
        "remote",
        Some(&local),
    )
    .expect("ask filed");
    store
        .answer_user_interaction(&UserInteractionAnswer {
            interaction_id: ask_id.clone(),
            answer_option_id: Some(egress_ask_options::RUN_LOCAL.to_string()),
            answer_text: None,
            answered_by: "test-operator".to_string(),
        })
        .expect("answered");
    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    apply_egress_ask_declassification(&store, &it).expect("no-op");
    assert!(
        !session_sink_declassified(&store, "sess-pin-ask", "sess-pin-ask", Sink::RemoteModel),
        "run_local must not widen egress"
    );
}

#[test]
fn provider_selected_carries_the_inline_ask_outcome_payload() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);

    let plan = plan_taint_following_route(
        &EgressLabel::local_only(),
        Some(EgressClass::Remote),
        &[cand("local", EgressClass::Local), cand("remote", EgressClass::Remote)],
        None,
    );
    let local = cand("local", EgressClass::Local);
    let inline = serde_json::json!({
        "status": "answered",
        "outcome": egress_ask_options::RUN_LOCAL,
        "interaction_id": "ui-xyz",
    });
    emit_provider_selected(
        &store,
        "sess-pin-ask",
        "coder.default",
        Some("turn-000007"),
        &plan,
        Some(&local.name),
        &[],
        false,
        false,
        Some(&inline),
    );

    let payloads = provider_selected_payloads(&store);
    let ev = payloads.first().expect("one event");
    assert_eq!(ev["inline_ask"]["status"], "answered");
    assert_eq!(ev["inline_ask"]["outcome"], "run_local");
    assert_eq!(ev["inline_ask"]["interaction_id"], "ui-xyz");
    assert_eq!(ev["chosen_preset"], "local");

    // A plain emission (no ask) carries null.
    emit_provider_selected(
        &store,
        "sess-pin-ask",
        "coder.default",
        Some("turn-000008"),
        &EgressRoutingPlan {
            batch: EgressLabel::local_only(),
            provider_constraint: None,
            primary_eligible: false,
            eligible: vec![],
            reroute_to: None,
        },
        None,
        &[],
        false,
        false,
        None,
    );
    let payloads2 = provider_selected_payloads(&store);
    let ev2 = payloads2.first().expect("newest event");
    assert!(ev2["inline_ask"].is_null());
}
