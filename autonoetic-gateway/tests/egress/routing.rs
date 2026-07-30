//! Integration: taint-following routing → `egress.provider_selected` events.
//!
//! RFC data-envelopes §5.3 (taint-following routing) + §9.1/§9.4 (traceability:
//! "why did turn N run on this provider?"). Phase 2 slice 3a (#907).
//!
//! The pure routing *decision* (which presets are eligible, whether to reroute,
//! or refuse) is unit-tested in `egress_labeler`. This test verifies the
//! **audit boundary**: that `egress.provider_selected` is persisted to the
//! causal chain with the content-free payload an operator needs to reconstruct
//! the decision — the eligible set, the chosen preset, the batch label, and the
//! no-eligible-provider refusal. It drives the labeler helpers directly against
//! a real `GatewayStore` (tempfile-isolated), the same integration boundary the
//! `egress_source_rules` suite uses; the full §5.6 session e2e lands in slice 6.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{
    emit_provider_selected, plan_taint_following_route, PresetCandidate,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressClass, EgressLabel};

fn cand(name: &str, class: EgressClass) -> PresetCandidate {
    PresetCandidate {
        name: name.to_string(),
        egress_class: Some(class),
    }
}

fn provider_selected_events(
    store: &GatewayStore,
    session_id: &str,
) -> Vec<autonoetic_types::causal_chain::CausalEventRecord> {
    store
        .search_causal_events(Some(session_id), None, 50)
        .expect("search_causal_events")
        .into_iter()
        .filter(|e| e.action == "egress.provider_selected")
        .collect()
}

fn payload(e: &autonoetic_types::causal_chain::CausalEventRecord) -> serde_json::Value {
    serde_json::from_str(e.payload.as_deref().expect("payload")).expect("json payload")
}

#[test]
fn tainted_batch_reroute_is_answerable_from_the_chain() -> anyhow::Result<()> {
    // The email turn: a local_only batch, a remote primary, and a local preset
    // configured → reroute to local. The operator must be able to answer "why
    // did this turn run on ollama?" from `egress.provider_selected` alone.
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let presets = vec![
        cand("sonnet", EgressClass::Remote),
        cand("ollama", EgressClass::Local),
    ];
    let plan = plan_taint_following_route(
        &EgressLabel::local_only(),
        Some(EgressClass::Remote),
        &presets,
            None,
        );
    assert!(!plan.primary_eligible);
    assert_eq!(plan.reroute_to.as_ref().map(|c| c.name.as_str()), Some("ollama"));

    // The remote fallback preset was skipped as ineligible.
    emit_provider_selected(
        &store,
        "sess-mail",
        "mail.default",
        Some("turn-000004"),
        &plan,
        Some("ollama"),
        &["sonnet-fallback".to_string()],
        true,
        false,
    );

    let events = provider_selected_events(&store, "sess-mail");
    assert_eq!(events.len(), 1, "exactly one provider_selected per completion");
    let p = payload(&events[0]);
    assert_eq!(p["batch_label_name"], "local_only");
    assert_eq!(p["chosen_preset"], "ollama");
    assert_eq!(p["primary_eligible"], false);
    assert_eq!(p["rerouted"], true);
    assert_eq!(p["no_eligible_provider"], false);
    assert_eq!(p["eligible_presets"], serde_json::json!(["ollama"]));
    assert_eq!(p["fallback_skipped"], serde_json::json!(["sonnet-fallback"]));
    // Content-free: the event target is the chosen preset, never content.
    assert_eq!(events[0].target.as_deref(), Some("ollama"));
    // Egress category so the audit view can group it.
    assert_eq!(events[0].category, "egress");
    Ok(())
}

#[test]
fn no_eligible_provider_refusal_is_recorded() -> anyhow::Result<()> {
    // local_only batch, remote primary, and NO local preset → no eligible
    // provider. The refusal must be auditable (chosen = none), so the operator
    // sees the turn refused rather than silently shipping taint.
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let presets = vec![cand("sonnet", EgressClass::Remote)];
    let plan = plan_taint_following_route(
        &EgressLabel::local_only(),
        Some(EgressClass::Remote),
        &presets,
            None,
        );
    assert!(plan.no_eligible_provider());

    emit_provider_selected(
        &store,
        "sess-refuse",
        "mail.default",
        Some("turn-000004"),
        &plan,
        None, // refused — no chosen preset
        &[],
        false,
        false,
    );

    let events = provider_selected_events(&store, "sess-refuse");
    assert_eq!(events.len(), 1);
    let p = payload(&events[0]);
    assert_eq!(p["no_eligible_provider"], true);
    assert_eq!(p["chosen_preset"], serde_json::Value::Null);
    assert_eq!(p["eligible_presets"], serde_json::json!([]));
    assert_eq!(p["batch_label_name"], "local_only");
    assert_eq!(events[0].target.as_deref(), Some("none"));
    Ok(())
}

#[test]
fn clean_batch_needs_no_event() {
    // An unrestricted batch is the fast no-op: no reroute, no refusal — and the
    // lifecycle skips emission entirely, keeping the causal chain free of noise
    // for ordinary clean turns. Here we just assert the plan shape the caller
    // uses to make that skip decision.
    let plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &[cand("ollama", EgressClass::Local)],
            None,
        );
    assert!(plan.primary_eligible);
    assert!(plan.reroute_to.is_none());
    assert!(!plan.no_eligible_provider());
}

#[test]
fn provider_constraint_local_only_reroutes_clean_batches() -> anyhow::Result<()> {
    // RFC §5.4 rung 1: a constrained session restricts provider *selection*,
    // not just content — a clean batch on a remote primary reroutes to local.
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let presets = vec![
        cand("sonnet", EgressClass::Remote),
        cand("ollama", EgressClass::Local),
    ];
    let plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        Some(autonoetic_types::egress::ProviderConstraint::LocalOnly),
    );
    assert!(!plan.primary_eligible);
    assert_eq!(
        plan.reroute_to.as_ref().map(|c| c.name.as_str()),
        Some("ollama")
    );
    assert_eq!(
        plan.batch,
        EgressLabel::local_only(),
        "effective batch is the constraint intersection"
    );
    assert_eq!(
        plan.provider_constraint,
        Some(autonoetic_types::egress::ProviderConstraint::LocalOnly)
    );

    // The audit event names the constraint, so "why did this clean turn run
    // on ollama?" is answerable from the chain.
    emit_provider_selected(
        &store,
        "sess-private",
        "mail.default",
        Some("turn-000001"),
        &plan,
        Some("ollama"),
        &[],
        true,
        false,
    );
    let events = provider_selected_events(&store, "sess-private");
    assert_eq!(events.len(), 1);
    let p = payload(&events[0]);
    assert_eq!(p["provider_constraint"], "local_only");
    assert_eq!(p["chosen_preset"], "ollama");

    // Without the constraint the same clean batch is a no-op.
    let unconstrained = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        None,
    );
    assert!(unconstrained.primary_eligible);
    assert!(unconstrained.reroute_to.is_none());
    Ok(())
}

#[test]
fn provider_constraint_local_only_without_local_preset_refuses() {
    // A constrained session with no local preset refuses even a clean turn —
    // a refused turn beats a remote leak (RFC §5.4 fail-closed).
    let presets = vec![cand("sonnet", EgressClass::Remote)];
    let plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Remote),
        &presets,
        Some(autonoetic_types::egress::ProviderConstraint::LocalOnly),
    );
    assert!(plan.no_eligible_provider());
    assert!(plan.eligible.is_empty());
}

#[test]
fn provider_constraint_local_only_keeps_local_primary() {
    let presets = vec![cand("ollama", EgressClass::Local)];
    let plan = plan_taint_following_route(
        &EgressLabel::unrestricted(),
        Some(EgressClass::Local),
        &presets,
        Some(autonoetic_types::egress::ProviderConstraint::LocalOnly),
    );
    assert!(plan.primary_eligible);
    assert!(plan.reroute_to.is_none());
}
