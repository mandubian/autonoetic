//! Constitutional coverage for §15 Data Egress Localization + I-14
//! (`docs/constitution/versions/2026.07.30/`, #910).
//!
//! Mechanics-first amendment: the enforcement shipped and was tested under
//! `tests/egress/*` across phases 1–4 (#904–#909, follow-ups #961–#966)
//! before the clause was written. This module pins the *constitutional*
//! surface:
//! - text presence of the new clauses (a clause without a test is a lie),
//! - enforcement-register coverage with resolvable citations,
//! - fail-mode declarations (I-11),
//! - behavioral rule-ID attribution: emitted egress events carry their §15
//!   clause in `enforced_rules` (I-6), not just the baseline attribution rule.

use autonoetic_gateway::enforcement_register::{clause_of_rule, entries_for};
use autonoetic_gateway::fail_mode::{lookup_fail_mode, FailMode};
use autonoetic_gateway::runtime::egress_labeler::{
    emit_declassified, emit_provider_selected, emit_surface_boundary_refused,
    plan_taint_following_route, session_network_declass_target, PresetCandidate,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::GrantScope;
use autonoetic_types::egress::{EgressClass, EgressLabel, Sink};
use std::sync::Arc;

const CONSTITUTION: &str =
    include_str!("../../../docs/constitution/versions/2026.07.30/constitution.md");

#[test]
fn section_15_and_i_14_are_present() {
    assert!(
        CONSTITUTION.contains("## 15. Data Egress Localization"),
        "expected the new §15 section header"
    );
    for id in ["P-15.1", "P-15.2", "P-15.3"] {
        assert!(
            CONSTITUTION.contains(&format!("| {id} |")),
            "expected a {id} row in §15"
        );
    }
    assert!(
        CONSTITUTION.contains("I-14"),
        "expected the I-14 invariant bullet in §13"
    );
    // The ratio prose keeps pace with the mechanically counted lock numbers:
    // P-15.1/15.2/15.3 raise rules 179 → 182.
    assert!(
        CONSTITUTION.contains("182 rules"),
        "rights/rules ratio prose should read 182 rules after the §15 additions"
    );
}

#[test]
fn section_15_rules_are_enforced_not_missing() {
    let section_start = CONSTITUTION
        .find("## 15. Data Egress Localization")
        .expect("§15 must exist");
    let section_end = CONSTITUTION[section_start..]
        .find("## Amendment process")
        .map(|i| section_start + i)
        .expect("Amendment process follows §15");
    let section = &CONSTITUTION[section_start..section_end];
    for id in ["P-15.1", "P-15.2", "P-15.3"] {
        let row = section
            .lines()
            .find(|l| l.starts_with(&format!("| {id} |")))
            .unwrap_or_else(|| panic!("missing row for {id}"));
        assert!(
            row.trim_end().ends_with("| ENFORCED |"),
            "{id} must be declared ENFORCED (mechanics shipped before the clause), got: {row}"
        );
    }
}

#[test]
fn register_covers_the_section_15_rules() {
    let entries: Vec<_> = entries_for("P-15").collect();
    assert_eq!(entries.len(), 3, "P-15.1/15.2/15.3 each need an entry");
    for rule in ["P-15.1", "P-15.2", "P-15.3"] {
        assert!(
            entries.iter().any(|e| e.rule_id == rule),
            "missing register entry for {rule}"
        );
        assert_eq!(
            clause_of_rule(rule),
            Some("P-15"),
            "{rule} must attribute to the P-15 principle for contract-health"
        );
    }
    // The register's own `every_parseable_citation_resolves` test pins the
    // code/test citations these entries carry.
}

#[test]
fn fail_modes_are_declared_for_section_15() {
    for rule in ["P-15.1", "P-15.2", "P-15.3"] {
        assert_eq!(
            lookup_fail_mode(rule),
            Some(FailMode::RefuseTurn),
            "{rule} must declare refuse-turn (mid-turn refusal, I-11)"
        );
    }
}

/// Behavioral: every emitted egress event carries its §15 clause in
/// `enforced_rules` — the I-6 attribution surface for the label plane.
#[test]
fn egress_events_carry_their_section_15_clause() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let session = "root-p15/coder";

    // P-15.2 — boundary refusal.
    emit_surface_boundary_refused(
        &store,
        session,
        "coder.default",
        Some("turn-1"),
        "web",
        &EgressLabel::local_only(),
        &[],
        "constitution test refusal",
    );
    // P-15.3 — declassification.
    emit_declassified(
        &store,
        session,
        "coder.default",
        &session_network_declass_target("root-p15"),
        Sink::Network,
        GrantScope::RootSession,
        None,
        "constitution test declass",
        None,
    );
    // P-15.1 — provider selection.
    let plan = plan_taint_following_route(
        &EgressLabel::local_only(),
        Some(EgressClass::Remote),
        &[PresetCandidate {
            name: "ollama".to_string(),
            egress_class: Some(EgressClass::Local),
        }],
        None,
    );
    emit_provider_selected(
        &store,
        session,
        "coder.default",
        Some("turn-1"),
        &plan,
        Some("ollama"),
        &[],
        true,
        false,
        None,
    );

    let events = store.search_causal_events(Some(session), None, 50)?;
    let clause_for = |action: &str| {
        events
            .iter()
            .find(|e| e.action == action)
            .unwrap_or_else(|| panic!("missing event {action}"))
            .enforced_rules
            .clone()
    };
    assert!(clause_for("egress.boundary_refused").contains(&"P-15.2".to_string()));
    assert!(clause_for("egress.declassified").contains(&"P-15.3".to_string()));
    assert!(clause_for("egress.provider_selected").contains(&"P-15.1".to_string()));
    // The baseline attribution rule rides along on every one of them (I-6).
    for action in [
        "egress.boundary_refused",
        "egress.declassified",
        "egress.provider_selected",
    ] {
        assert!(
            clause_for(action)
                .contains(&autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION.to_string()),
            "{action} must keep the baseline attribution rule"
        );
    }
    Ok(())
}
