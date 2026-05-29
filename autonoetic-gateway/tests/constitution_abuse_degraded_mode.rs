//! Constitution P-7.18 — Degraded session mode.
//!
//! A `Degraded` session state sits between healthy and emergency-stopped.
//! Tool-tier filter clamps to Core, `sandbox_exec`/`artifact_exec` are
//! refused regardless of tier, and loop-guard sub-trip warnings auto-enter
//! degraded mode. Exit requires operator clearance.

mod support;

use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::runtime::tools::ToolTierFilter;
use autonoetic_types::agent::SessionState;

#[test]
fn degraded_tier_filter_clamps_to_core_only() {
    let filter = ToolTierFilter::core_only();
    assert!(filter.allows("content_write"), "core content allowed");
    assert!(filter.allows("knowledge_store"), "core knowledge allowed");
    assert!(filter.allows("artifact_build"), "core artifact allowed");
    assert!(!filter.allows("agent_spawn"), "workflow blocked");
    assert!(!filter.allows("web_search"), "specialized blocked");
    assert!(!filter.allows("promotion_record"), "specialized blocked");
    assert!(
        !filter.allows("agent_revision_create"),
        "specialized blocked"
    );
    assert!(
        filter.allows("sandbox_exec"),
        "sandbox_exec is core tier — blocked by tool-level check instead"
    );
}

#[test]
fn sub_trip_warning_triggers_at_80_percent_budget() {
    let mut guard = LoopGuard::new(5);
    assert!(!guard.is_sub_trip_warning(), "no warning at start");

    for _ in 0..3 {
        guard.check_loop().unwrap();
    }
    assert!(
        !guard.is_sub_trip_warning(),
        "3/5 loops — below 80% threshold"
    );

    guard.check_loop().unwrap();
    assert!(guard.is_sub_trip_warning(), "4/5 loops — at 80% threshold");
}

#[test]
fn sub_trip_warning_clears_after_trip() {
    let mut guard = LoopGuard::new(5);
    for _ in 0..4 {
        guard.check_loop().unwrap();
    }
    assert!(guard.is_sub_trip_warning(), "at 80% threshold");

    guard.check_loop().unwrap(); // 5th loop still passes (4 < 5)
    assert!(
        !guard.is_sub_trip_warning(),
        "at 5/5 — threshold is 4, current is 5"
    );

    assert!(guard.check_loop().is_err(), "6th loop trips (5 >= 5)");
}

#[test]
fn sub_trip_warning_on_tool_failures() {
    let mut guard = LoopGuard::new(100);
    for _ in 0..3 {
        guard.register_failure("failing_tool", "{}", None);
    }
    assert!(!guard.is_sub_trip_warning(), "3/5 failures — below 80%");

    guard.register_failure("failing_tool", "{}", None);
    assert!(
        guard.is_sub_trip_warning(),
        "4/5 failures — at 80% threshold"
    );
}

#[test]
fn sub_trip_ignores_permission_errors() {
    use autonoetic_types::tool_error::ToolErrorType;
    let mut guard = LoopGuard::new(5);
    for _ in 0..10 {
        guard.register_failure("denied_tool", "{}", Some(&ToolErrorType::Permission));
    }
    assert!(
        !guard.is_sub_trip_warning(),
        "permission errors excluded from budget"
    );
}

#[test]
fn session_state_default_is_normal() {
    assert_eq!(SessionState::default(), SessionState::Normal);
}

#[test]
fn session_state_serde_roundtrip() {
    let state = autonoetic_types::agent::SessionState::Degraded;
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(json, "\"degraded\"");
    let parsed: autonoetic_types::agent::SessionState = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, state);
}
