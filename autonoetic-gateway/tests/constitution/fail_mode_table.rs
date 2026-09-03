//! Constitution I-11: Unified fail-mode table.
//!
//! Every constitutional invariant has a declared failure action in one
//! place.  The five modes are: refuse-boot, refuse-session-start,
//! degrade, emergency-stop, log-only.  This test validates:
//!
//! 1. The fail-mode table covers every rule in the constitution.
//! 2. No rule is missing a fail-mode entry.
//! 3. Specific archetype rules have the correct fail-mode (e.g. P-6.5
//!    is refuse-session-start, not log-only).
//! 4. P-6.5 cost-budget catalog unavailability is enforced (no silent-disable).

use autonoetic_gateway::fail_mode::{
    all_entries, entries_by_fail_mode, lookup_fail_mode, FailMode,
};
use autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry;
use autonoetic_types::config::SessionBudgetConfig;

const CONSTITUTION_RULE_IDS: &[&str] = &[
    // §0 Rights
    "Ri-0.1", "Ri-0.2", "Ri-0.3", "Ri-0.4", "Ri-0.5", "Ri-0.6", "Ri-0.7", "Ri-0.8", "Ri-0.9",
    "Ri-0.10", "Ri-0.11", "Ri-0.12", "Ri-0.13", // §1 Capability & Rights
    "P-1.1", "P-1.2", "P-1.3", "P-1.4", "P-1.5", "P-1.6", "P-1.7", "P-1.8", "P-1.9", "P-1.10",
    "P-1.11", // §2 Approval Gates
    "P-2.1", "P-2.2", "P-2.3", "P-2.4", "P-2.5", "P-2.6", "P-2.7", "P-2.8", "P-2.9", "P-2.10",
    "P-2.11", "P-2.12", "P-2.13", "P-2.14", "P-2.15", "P-2.16", "P-2.17", "Ri-0.14", "Ri-0.15",
    "Ri-0.16", "Ri-0.17", "Ri-0.18", "P-2.23", "P-2.24", // §3 Sandbox Isolation
    "P-3.1", "P-3.2", "P-3.3", "P-3.4", "P-3.5", "P-3.6", "P-3.7", "P-3.8", "P-3.9", "P-3.10",
    // §4 Credential & Secret Protection
    "P-4.1", "P-4.2", "P-4.3", "P-4.4", "P-4.5", "P-4.6", "P-4.7", "P-4.8", "P-4.9", "P-4.10",
    "P-4.11", "P-4.12", "P-4.13", "P-4.14", // §5 I/O Schema Validation
    "P-5.1", "P-5.2", "P-5.3", "P-5.4", "P-5.5", "P-5.6", "P-5.7", "P-5.8", "P-5.9", "P-5.10",
    "P-5.11", "P-5.12", "P-5.13", // §6 Session, Workflow & Budget
    "P-6.1", "P-6.2", "P-6.3", "P-6.4", "P-6.5", "P-6.6", "P-6.7", "P-6.8", "P-6.9", "P-6.10",
    "P-6.11", "P-6.12", "P-6.13", "P-6.14", "P-6.15", "P-6.16", "P-6.17", "P-6.18", "P-6.19",
    "P-6.20", "P-6.21", "P-6.22", "P-6.23", "P-4.15",
    // §7 Abuse / Hard-Stop / Circuit Breakers
    "P-7.1", "P-7.2", "P-7.3", "P-7.4", "P-7.5", "P-7.6", "P-7.7", "P-7.8", "P-7.9", "P-7.10",
    "P-7.11", "P-7.12", "P-7.13", "P-7.14", "P-7.15", "P-7.16", "P-7.17", "P-7.18",
    // §8 Audit & Traceability
    "P-8.1", "P-8.2", "P-8.3", "P-8.4", "P-8.5", "P-8.6", "P-8.7", "P-8.8", "P-8.9", "P-8.10",
    "P-8.11", "P-8.12", "P-8.13", "P-8.14", "P-8.15", "P-8.16", "P-8.17", "P-8.18",
    // §9 Agent Install & Provenance
    "P-9.1", "P-9.2", "P-9.3", "P-9.4", "P-9.5", "P-9.6", "P-9.7", "P-9.8", "P-9.9", "P-9.10",
    "P-9.11", "P-9.12", "P-9.13", "P-9.14", // §10 Federation / Remote
    "P-10.1", "P-10.2", "P-10.3", "P-10.4", "P-10.5", "P-10.6", "P-10.7", "P-10.8", "P-10.9",
    // §11 Inter-Agent Messaging
    "P-11.1", "P-11.2", "P-11.3", "P-11.4", "P-11.5", "P-11.6", "P-11.7", "P-11.8", "P-7.21",
    "P-7.22", // §13 Cross-cutting invariants
    "I-6", "I-10", "I-11",
];

#[test]
fn r_plus_plus_10_every_constitutional_rule_has_fail_mode() {
    let mut missing: Vec<&str> = Vec::new();
    for rule_id in CONSTITUTION_RULE_IDS {
        if lookup_fail_mode(rule_id).is_none() {
            missing.push(rule_id);
        }
    }
    assert!(
        missing.is_empty(),
        "I-11: the following rules are missing fail-mode entries: {:?}",
        missing
    );
}

#[test]
fn r_plus_plus_10_no_duplicate_entries() {
    let entries = all_entries();
    let mut seen = std::collections::HashSet::new();
    let mut dupes = Vec::new();
    for (rule_id, _) in &entries {
        if !seen.insert(rule_id) {
            dupes.push(rule_id);
        }
    }
    assert!(
        dupes.is_empty(),
        "I-11: duplicate fail-mode entries for: {:?}",
        dupes
    );
}

#[test]
fn r_plus_plus_10_all_five_modes_represented() {
    let modes: Vec<FailMode> = all_entries().iter().map(|(_, m)| *m).collect();
    assert!(
        modes.contains(&FailMode::RefuseBoot),
        "must have refuse-boot entries"
    );
    assert!(
        modes.contains(&FailMode::RefuseSessionStart),
        "must have refuse-session-start entries"
    );
    assert!(
        modes.contains(&FailMode::Degrade),
        "must have degrade entries"
    );
    assert!(
        modes.contains(&FailMode::EmergencyStop),
        "must have emergency-stop entries"
    );
    assert!(
        modes.contains(&FailMode::LogOnly),
        "must have log-only entries"
    );
}

#[test]
fn r_plus_plus_10_r_6_5_is_refuse_session_start() {
    let mode = lookup_fail_mode("P-6.5").expect("P-6.5 must have a fail-mode entry");
    assert_eq!(
        mode,
        FailMode::RefuseSessionStart,
        "P-6.5 (max_session_price_usd) must be refuse-session-start, not log-only"
    );
}

#[test]
fn r_plus_plus_10_r_7_18_is_degrade() {
    let mode = lookup_fail_mode("P-7.18").expect("P-7.18 must have a fail-mode entry");
    assert_eq!(mode, FailMode::Degrade);
}

#[test]
fn r_plus_plus_10_r_plus_plus_8_escape_is_degrade() {
    let mode = lookup_fail_mode("P-7.22").expect("P-7.22 must have a fail-mode entry");
    assert_eq!(mode, FailMode::Degrade);
}

#[test]
fn r_plus_plus_10_catalog_unavailable_refuses_cost_budgeted_session() {
    let registry = SessionBudgetRegistry::new(SessionBudgetConfig {
        max_session_price_usd: Some(1.0),
        ..Default::default()
    });

    let result = registry.record_llm_completion("scope-1", 100, 50, None);
    assert!(
        result.is_err(),
        "P-6.5/I-11: record_llm_completion with None cost must fail when max_session_price_usd is set"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("P-6.5") || err.contains("catalog is unavailable"),
        "error message must reference P-6.5 or catalog unavailability, got: {}",
        err
    );
}

#[test]
fn r_plus_plus_10_catalog_available_succeeds_cost_budgeted_session() {
    let registry = SessionBudgetRegistry::new(SessionBudgetConfig {
        max_session_price_usd: Some(1.0),
        ..Default::default()
    });

    let result = registry.record_llm_completion("scope-2", 100, 50, Some(0.001));
    assert!(
        result.is_ok(),
        "record_llm_completion with valid cost estimate must succeed"
    );
}

#[test]
fn r_plus_plus_10_no_price_limit_allows_none_cost() {
    let registry = SessionBudgetRegistry::new(SessionBudgetConfig {
        max_session_price_usd: None,
        ..Default::default()
    });

    let result = registry.record_llm_completion("scope-3", 100, 50, None);
    assert!(
        result.is_ok(),
        "without max_session_price_usd, None cost must be accepted"
    );
}

#[test]
fn r_plus_plus_10_lookup_returns_none_for_unknown_rule() {
    assert!(lookup_fail_mode("R-999").is_none());
}

#[test]
fn r_plus_plus_10_negative_price_limit_allows_none_cost() {
    let registry = SessionBudgetRegistry::new(SessionBudgetConfig {
        max_session_price_usd: Some(-1.0),
        ..Default::default()
    });

    let result = registry.record_llm_completion("scope-neg", 100, 50, None);
    assert!(
        result.is_ok(),
        "negative max_session_price_usd must not trigger catalog-unavailable enforcement"
    );
}

#[test]
fn r_plus_plus_10_entries_by_fail_mode_returns_correct_subset() {
    let degrade_entries = entries_by_fail_mode(FailMode::Degrade);
    assert!(
        degrade_entries.contains(&"P-7.18"),
        "P-7.18 must be in degrade entries"
    );
    assert!(
        degrade_entries.contains(&"P-7.22"),
        "P-7.22 must be in degrade entries"
    );
    assert!(
        !degrade_entries.contains(&"P-7.1"),
        "P-7.1 must NOT be in degrade entries (it is emergency-stop)"
    );
}
