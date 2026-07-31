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
    emit_provider_selected, file_egress_pin_ask, is_egress_pin_ask, PinAskBlocker,
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
        PinAskBlocker::BatchTaint,
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
        egress_ask_options_payload(PinAskBlocker::BatchTaint, Some(&local)),
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
        PinAskBlocker::BatchTaint,
    )
    .expect("ask filed");
    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["declassify", "abort"]);
    assert_eq!(
        egress_ask_options_payload(PinAskBlocker::BatchTaint, None),
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
        PinAskBlocker::BatchTaint,
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
        PinAskBlocker::BatchTaint,
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
        PinAskBlocker::BatchTaint,
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
        PinAskBlocker::BatchTaint,
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

// ── PR #996 review: the provider_constraint blocker ────────────────────────
//
// A `provider_constraint` (RFC §5.4 rung 1) blocks a pinned preset by operator
// decree and outranks declassification grants. Offering "declassify" for that
// conflict files an ask, materializes a grant that changes nothing, and re-files
// the ask on resume — indefinitely. These pin the shape that prevents it.

/// The constraint-blocked ask must not offer a declassification it cannot honor.
#[test]
fn provider_constraint_ask_omits_declassify() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);

    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000009",
        // A *clean* batch: this conflict needs no taint at all, only a room
        // pinned local plus a pinned remote preset.
        &EgressLabel::unrestricted(),
        "remote",
        Some(&local),
        PinAskBlocker::ProviderConstraint,
    )
    .expect("ask filed");

    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        ids,
        ["run_local", "abort"],
        "declassify cannot lift a room pin, so it must not be offered"
    );
    assert!(
        !it.question.contains("declassify this root session"),
        "question must not promise a declassification: {}",
        it.question
    );
    // …and it must say what *does* lift it, or the operator is stuck.
    assert!(
        it.question.contains("/private"),
        "question should name the way out: {}",
        it.question
    );
}

/// The audit payload and the filed options are derived from one predicate, so
/// `egress.provider_selected` cannot claim an option the operator never saw.
#[test]
fn options_payload_matches_the_filed_options_per_blocker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let local = cand("local", EgressClass::Local);

    for (blocker, expected) in [
        (
            PinAskBlocker::BatchTaint,
            vec!["declassify", "run_local", "abort"],
        ),
        (PinAskBlocker::ProviderConstraint, vec!["run_local", "abort"]),
    ] {
        assert_eq!(egress_ask_options_payload(blocker, Some(&local)), expected);
    }
    // No local preset → no run_local, whichever the blocker.
    assert_eq!(
        egress_ask_options_payload(PinAskBlocker::BatchTaint, None),
        vec!["declassify", "abort"]
    );
    assert_eq!(
        egress_ask_options_payload(PinAskBlocker::ProviderConstraint, None),
        vec!["abort"],
        "abort is always present — the ask must never be a dead end"
    );

    // The filed interaction agrees with the payload helper.
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000010",
        &EgressLabel::local_only(),
        "remote",
        None,
        PinAskBlocker::ProviderConstraint,
    )
    .expect("ask filed");
    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    let ids: Vec<String> = it.options.iter().map(|o| o.id.clone()).collect();
    assert_eq!(
        ids,
        egress_ask_options_payload(PinAskBlocker::ProviderConstraint, None)
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
}

/// With no local preset configured, the question must not promise a local run.
#[test]
fn question_does_not_offer_a_local_run_without_a_local_preset() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000011",
        &EgressLabel::local_only(),
        "remote",
        None,
        PinAskBlocker::BatchTaint,
    )
    .expect("ask filed");
    let it = store
        .get_user_interaction(&ask_id)
        .expect("lookup")
        .expect("interaction");
    assert!(
        !it.question.contains("run this turn on"),
        "no local preset is offered, so the question must not mention running on one: {}",
        it.question
    );
    let ids: Vec<&str> = it.options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["declassify", "abort"]);
}

/// The blocker is recorded in the interaction context, so the audit and the
/// resume side can tell *why* the pin was blocked rather than only that it was.
#[test]
fn blocker_is_recorded_in_the_interaction_context() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    for (blocker, expected) in [
        (PinAskBlocker::BatchTaint, "batch_taint"),
        (PinAskBlocker::ProviderConstraint, "provider_constraint"),
    ] {
        let tmp2 = tempfile::tempdir().expect("tempdir");
        let s2 = store_in(&tmp2);
        let store_ref = if expected == "batch_taint" { &store } else { &s2 };
        let ask_id = file_egress_pin_ask(
            store_ref,
            "sess-pin-ask",
            "sess-pin-ask",
            "coder.default",
            "turn-000012",
            &EgressLabel::local_only(),
            "remote",
            None,
            blocker,
        )
        .expect("ask filed");
        let it = store_ref
            .get_user_interaction(&ask_id)
            .expect("lookup")
            .expect("interaction");
        let ctx: serde_json::Value =
            serde_json::from_str(it.context.as_deref().unwrap_or("{}")).expect("context json");
        assert_eq!(ctx["blocker"], expected);
    }
}

/// A declassify answer on a constraint-blocked ask must be inert: the option is
/// not offered, and even a hand-crafted answer must not materialize a grant that
/// would silently widen the room the operator pinned local.
#[test]
fn declassify_is_not_materialized_for_a_constraint_blocked_ask() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = store_in(&tmp);
    let ask_id = file_egress_pin_ask(
        &store,
        "sess-pin-ask",
        "sess-pin-ask",
        "coder.default",
        "turn-000013",
        &EgressLabel::unrestricted(),
        "remote",
        None,
        PinAskBlocker::ProviderConstraint,
    )
    .expect("ask filed");

    // `declassify` was never offered, so answering with it is rejected by the
    // interaction's own option validation — the operator cannot get here through
    // the UI, and a replay cannot either.
    let answered = store.answer_user_interaction(&UserInteractionAnswer {
        interaction_id: ask_id.clone(),
        answer_option_id: Some(egress_ask_options::DECLASSIFY.to_string()),
        answer_text: None,
        answered_by: "operator:test".to_string(),
    });
    assert!(
        answered.is_err(),
        "an option that was never offered must not be answerable"
    );
    assert!(
        !session_sink_declassified(&store, "sess-pin-ask", "sess-pin-ask", Sink::RemoteModel),
        "no grant may exist for a constraint-blocked ask"
    );
}
