//! Constitution R++10: Unified fail-mode table.
//!
//! Every constitutional invariant has a declared failure action in one
//! place.  The five modes are: refuse-boot, refuse-session-start,
//! degrade, emergency-stop, log-only.  This test validates:
//!
//! 1. The fail-mode table covers every rule in the constitution.
//! 2. No rule is missing a fail-mode entry.
//! 3. Specific archetype rules have the correct fail-mode (e.g. R-6.5
//!    is refuse-session-start, not log-only).
//! 4. R-6.5 cost-budget catalog unavailability is enforced (no silent-disable).

use autonoetic_gateway::fail_mode::{
    all_entries, entries_by_fail_mode, lookup_fail_mode, FailMode,
};
use autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry;
use autonoetic_types::config::SessionBudgetConfig;

const CONSTITUTION_RULE_IDS: &[&str] = &[
    // §0 Rights
    "Ri-0.1", "Ri-0.2", "Ri-0.3", "Ri-0.4", "Ri-0.5", "Ri-0.6",
    "Ri-0.7", "Ri-0.8", "Ri-0.9", "Ri-0.10", "Ri-0.11", "Ri-0.12",
    "Ri-0.13",
    // §1 Capability & Rights
    "R-1.1", "R-1.2", "R-1.3", "R-1.4", "R-1.5", "R-1.6", "R-1.7",
    "R-1.8", "R-1.9", "R-1.10", "R-1.11",
    // §2 Approval Gates
    "R-2.1", "R-2.2", "R-2.3", "R-2.4", "R-2.5", "R-2.6", "R-2.7",
    "R-2.8", "R-2.9", "R-2.10", "R-2.11", "R-2.12", "R-2.13", "R-2.14",
    "R-2.15", "R-2.16", "R-2.17",
    // §3 Sandbox Isolation
    "R-3.1", "R-3.2", "R-3.3", "R-3.4", "R-3.5", "R-3.6", "R-3.7",
    "R-3.8", "R-3.9",
    // §4 Credential & Secret Protection
    "R-4.1", "R-4.2", "R-4.3", "R-4.4", "R-4.5", "R-4.6", "R-4.7",
    "R-4.8", "R-4.9", "R-4.10", "R-4.11", "R-4.12", "R-4.13", "R-4.14",
    // §5 I/O Schema Validation
    "R-5.1", "R-5.2", "R-5.3", "R-5.4", "R-5.5", "R-5.6", "R-5.7",
    "R-5.8", "R-5.9", "R-5.10", "R-5.11", "R-5.12", "R-5.13",
    // §6 Session, Workflow & Budget
    "R-6.1", "R-6.2", "R-6.3", "R-6.4", "R-6.5", "R-6.6", "R-6.7",
    "R-6.8", "R-6.9", "R-6.10", "R-6.11", "R-6.12", "R-6.13", "R-6.14",
    "R-6.15", "R-6.16", "R-6.17", "R-6.18", "R-6.19", "R-6.20",
    "R-6.21", "R-6.22", "R-6.23",
    // §7 Abuse / Hard-Stop / Circuit Breakers
    "R-7.1", "R-7.2", "R-7.3", "R-7.4", "R-7.5", "R-7.6", "R-7.7",
    "R-7.8", "R-7.9", "R-7.10", "R-7.11", "R-7.12", "R-7.13", "R-7.14",
    "R-7.15", "R-7.16", "R-7.17", "R-7.18",
    // §8 Audit & Traceability
    "R-8.1", "R-8.2", "R-8.3", "R-8.4", "R-8.5", "R-8.6", "R-8.7",
    "R-8.8", "R-8.9", "R-8.10", "R-8.11", "R-8.12", "R-8.13", "R-8.14",
    "R-8.15", "R-8.16", "R-8.17", "R-8.18",
    // §9 Agent Install & Provenance
    "R-9.1", "R-9.2", "R-9.3", "R-9.4", "R-9.5", "R-9.6", "R-9.7",
    "R-9.8", "R-9.9", "R-9.10", "R-9.11", "R-9.12", "R-9.13", "R-9.14",
    // §10 Federation / Remote
    "R-10.1", "R-10.2", "R-10.3", "R-10.4", "R-10.5", "R-10.6",
    "R-10.7", "R-10.8",
    // §11 Inter-Agent Messaging
    "R-11.1", "R-11.2", "R-11.3", "R-11.4", "R-11.5", "R-11.6",
    "R-11.7", "R-11.8",
    // R+ additions
    "R+1", "R+2", "R+3", "R+4", "R+5", "R+6", "R+7", "R+8", "R+9",
    "R+10", "R+11", "R+12", "R+13", "R+14", "R+15", "R+16", "R+17",
    "R+18",
    // R++ additions
    "R++4", "R++7", "R++8", "R++9", "R++10",
    // R+++ additions
    "R+++1", "R+++2", "R+++3",
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
        "R++10: the following rules are missing fail-mode entries: {:?}",
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
        "R++10: duplicate fail-mode entries for: {:?}",
        dupes
    );
}

#[test]
fn r_plus_plus_10_all_five_modes_represented() {
    let modes: Vec<FailMode> = all_entries()
        .iter()
        .map(|(_, m)| *m)
        .collect();
    assert!(modes.contains(&FailMode::RefuseBoot), "must have refuse-boot entries");
    assert!(modes.contains(&FailMode::RefuseSessionStart), "must have refuse-session-start entries");
    assert!(modes.contains(&FailMode::Degrade), "must have degrade entries");
    assert!(modes.contains(&FailMode::EmergencyStop), "must have emergency-stop entries");
    assert!(modes.contains(&FailMode::LogOnly), "must have log-only entries");
}

#[test]
fn r_plus_plus_10_r_6_5_is_refuse_session_start() {
    let mode = lookup_fail_mode("R-6.5")
        .expect("R-6.5 must have a fail-mode entry");
    assert_eq!(
        mode,
        FailMode::RefuseSessionStart,
        "R-6.5 (max_session_price_usd) must be refuse-session-start, not log-only"
    );
}

#[test]
fn r_plus_plus_10_r_7_18_is_degrade() {
    let mode = lookup_fail_mode("R-7.18")
        .expect("R-7.18 must have a fail-mode entry");
    assert_eq!(mode, FailMode::Degrade);
}

#[test]
fn r_plus_plus_10_r_plus_plus_8_escape_is_degrade() {
    let mode = lookup_fail_mode("R++8")
        .expect("R++8 must have a fail-mode entry");
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
        "R-6.5/R++10: record_llm_completion with None cost must fail when max_session_price_usd is set"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("R-6.5") || err.contains("catalog is unavailable"),
        "error message must reference R-6.5 or catalog unavailability, got: {}",
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
        degrade_entries.contains(&"R-7.18"),
        "R-7.18 must be in degrade entries"
    );
    assert!(
        degrade_entries.contains(&"R++8"),
        "R++8 must be in degrade entries"
    );
    assert!(
        !degrade_entries.contains(&"R-7.1"),
        "R-7.1 must NOT be in degrade entries (it is emergency-stop)"
    );
}
