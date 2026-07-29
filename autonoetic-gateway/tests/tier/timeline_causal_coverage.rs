//! Coverage guard: maintains a curated list of operator-meaningful timeline
//! event types and verifies each one has an explicit altitude mapping above
//! Normal. When a new operator-meaningful event type is added to the system,
//! it should be registered in `TIMELINE_COVERAGE` so its altitude assignment
//! is mechanically verified. If the event type is intentionally excluded
//! (not session-scoped), document why in the module but do NOT add it to
//! `TIMELINE_COVERAGE`.


use autonoetic_gateway::runtime::session_timeline::base_altitude;
use autonoetic_types::session_timeline::Altitude;

const TIMELINE_COVERAGE: &[(&str, Altitude)] = &[
    ("session.emergency_stop", Altitude::Error),
    ("security.sandbox_escape", Altitude::Error),
    ("security.escape_threshold", Altitude::Attention),
    ("escalation.pending", Altitude::Attention),
    ("approval.pending", Altitude::Attention),
    ("user.ask.pending", Altitude::Attention),
    ("plan.pending", Altitude::Attention),
    ("divergence.intervention", Altitude::Attention),
    ("runtime.lock_drift", Altitude::Attention),
    ("guard.tripped", Altitude::Error),
    // #853: per-host sandbox_exec probe budget trip — operator triage signal.
    ("sandbox.host_budget_exhausted", Altitude::Attention),
    // #854: max_session_turns_hard termination (Error) and the advisory
    // continuation-window signal for a long-running delegated child (Attention).
    ("session.turn_hard_cap", Altitude::Error),
    ("session.continuation_window_extended", Altitude::Attention),
];

const INTENTIONALLY_EXCLUDED_CAUSAL_CATEGORIES: &[&str] = &[
    "sentinel",
    "capsule",
    "reclamation",
    "retention",
    "vault",
    "federation",
    "recording",
    "tool_call",
    "contract",
    "revision",
    "script",
    "url_literal",
    "ip_address",
    "import",
    "function_call",
    "network_command",
    "dependency_install",
    "reasoning",
    "curator",
    "agent.process",
    "loop_guard",
    "background",
];

#[test]
fn all_timeline_covered_event_types_have_correct_altitude() {
    for (event_type, expected_altitude) in TIMELINE_COVERAGE {
        let actual = base_altitude(event_type);
        assert_eq!(
            actual, *expected_altitude,
            "event type {:?} should be {:?}, got {:?}",
            event_type, expected_altitude, actual
        );
    }
}

#[test]
fn no_timeline_covered_event_type_falls_through_to_normal() {
    for (event_type, _) in TIMELINE_COVERAGE {
        let alt = base_altitude(event_type);
        assert_ne!(
            alt,
            Altitude::Normal,
            "event type {:?} must not fall through to Normal — \
             add it to the explicit match in base_altitude()",
            event_type
        );
    }
}

#[test]
fn coverage_list_covers_all_known_operator_meaningful_events() {
    let known: std::collections::HashSet<&str> = TIMELINE_COVERAGE
        .iter()
        .map(|(et, _)| *et)
        .collect();
    let excluded: std::collections::HashSet<&str> =
        INTENTIONALLY_EXCLUDED_CAUSAL_CATEGORIES.iter().copied().collect();

    for (event_type, _) in TIMELINE_COVERAGE {
        assert!(
            !excluded.contains(event_type),
            "event type {:?} is both in TIMELINE_COVERAGE and INTENTIONALLY_EXCLUDED — pick one",
            event_type
        );
    }

    assert!(
        known.contains("session.emergency_stop"),
        "emergency stop must be covered"
    );
    assert!(
        known.contains("security.sandbox_escape"),
        "sandbox escape must be covered"
    );
    assert!(
        known.contains("escalation.pending"),
        "escalation pending must be covered"
    );
    assert!(
        known.contains("security.escape_threshold"),
        "escape threshold (degradation) must be covered (#413)"
    );
    assert!(
        known.contains("guard.tripped"),
        "loop guard trip must be covered"
    );
    assert!(
        known.contains("runtime.lock_drift"),
        "runtime lock drift must be covered"
    );

    let _ = excluded;
}
