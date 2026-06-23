//! Sentinel P1 Integration Test: a looping session produces
//! `divergence.observed` then `divergence.detected` events in the
//! trajectory monitor's output.
//!
//! Uses a synthetic LoopGuard state to simulate a looping agent and
//! validates that the monitor's TickResult produces the expected
//! causal-action slugs for each TrajectoryHealth level.

use std::collections::HashMap;

use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::runtime::trajectory_health::{
    build_event_payload, DivergenceSignalKind, SignalSeverity, TrajectoryHealth,
};
use autonoetic_gateway::runtime::trajectory_monitor::{ToolObservation, TrajectoryMonitor};
use autonoetic_types::config::TrajectoryConfig;

fn cfg() -> TrajectoryConfig {
    TrajectoryConfig::default()
}

fn quiet_guard_state() -> LoopGuard {
    // Pin the pressure denominators so this test's arithmetic holds independent
    // of the production LoopGuard defaults (which are 10 / 8). The transitions
    // below are expressed as ratios — `current_loops`/5 and a tool failure
    // count/5 — so a warn signal fires at 4/5 = 0.80 and critical at 5/5 = 1.0.
    LoopGuard {
        max_loops_without_progress: 5,
        max_tool_failures: 5,
        ..LoopGuard::default()
    }
}

#[test]
fn looping_session_produces_divergence_event_sequence() {
    let mut mon = TrajectoryMonitor::new(cfg());
    let mut state = quiet_guard_state();

    // Turn 1: healthy, no event.
    let r = mon.tick(1, &[], None, &state);
    assert!(matches!(r.health, TrajectoryHealth::Healthy));
    assert!(r.level_changed);
    assert!(build_event_payload(&r.health).is_none());

    // Turns 2-3: still healthy.
    for turn in 2u64..=3 {
        state.current_loops = turn as u32;
        let r = mon.tick(turn, &[], None, &state);
        assert!(matches!(r.health, TrajectoryHealth::Healthy));
        assert!(!r.level_changed);
        assert!(build_event_payload(&r.health).is_none());
    }

    // Turn 4: loop pressure 4/5 = 0.80 (warn) → Watching → emits observed.
    state.current_loops = 4;
    let r = mon.tick(4, &[], None, &state);
    assert_eq!(r.health.level_str(), "watching");
    assert_eq!(r.health.causal_action(), Some("observed"));
    assert!(r.level_changed);
    let payload = build_event_payload(&r.health).unwrap();
    assert_eq!(payload["level"], "watching");

    // Turn 6: add failure pressure → 2 warn signals → Diverging → emits detected.
    state.current_loops = 4;
    state.tool_failure_counts.insert("sandbox.exec".into(), 4);
    let r = mon.tick(6, &[], None, &state);
    assert_eq!(r.health.level_str(), "diverging");
    assert_eq!(r.health.causal_action(), Some("detected"));
    assert!(r.level_changed);
    let payload = build_event_payload(&r.health).unwrap();
    assert_eq!(payload["level"], "diverging");

    // Turn 7: loop pressure critical → Critical → emits escalated.
    state.current_loops = 5;
    let r = mon.tick(7, &[], None, &state);
    assert_eq!(r.health.level_str(), "critical");
    assert_eq!(r.health.causal_action(), Some("escalated"));
    assert!(r.level_changed);
    let payload = build_event_payload(&r.health).unwrap();
    assert_eq!(payload["level"], "critical");

    // Turn 8: same critical → no re-trigger, no event emission.
    let r = mon.tick(8, &[], None, &state);
    assert_eq!(r.health.level_str(), "critical");
    assert!(r.health.causal_action().is_some());
    assert!(!r.level_changed, "same level must not re-trigger");
}

#[test]
fn healthy_session_emits_no_divergence_events() {
    let mut mon = TrajectoryMonitor::new(cfg());
    let state = quiet_guard_state();

    for turn in 1u64..=20 {
        let r = mon.tick(turn, &[], None, &state);
        assert!(matches!(r.health, TrajectoryHealth::Healthy));
        assert!(build_event_payload(&r.health).is_none(),
            "healthy session must not produce event payload");
    }
}

#[test]
fn disabled_monitor_produces_no_events_even_with_looping() {
    let mut cfg_disabled = cfg();
    cfg_disabled.enabled = false;
    let mut mon = TrajectoryMonitor::new(cfg_disabled);
    let mut state = quiet_guard_state();
    state.current_loops = 5; // would be critical if enabled

    let r = mon.tick(1, &[], None, &state);
    assert!(matches!(r.health, TrajectoryHealth::Healthy));
    assert!(!r.level_changed);
    assert!(build_event_payload(&r.health).is_none());
}

#[test]
fn signal_toggles_prevent_individual_signal_triggers() {
    let mut cfg_toggled = cfg();
    cfg_toggled.signals.loop_pressure = false;
    let mut mon = TrajectoryMonitor::new(cfg_toggled);
    let mut state = quiet_guard_state();
    state.current_loops = 5; // would be critical if loop_pressure were enabled

    let r = mon.tick(1, &[], None, &state);
    // No loop_pressure signal → still Healthy despite high loops.
    assert!(matches!(r.health, TrajectoryHealth::Healthy));
    assert!(r.level_changed);
    assert!(build_event_payload(&r.health).is_none());
}

#[test]
fn fingerprint_entropy_detects_repetition() {
    let mut mon = TrajectoryMonitor::new(cfg());

    // 4 identical fingerprints → entropy near 0. Repetition entropy is
    // ADVISORY: it surfaces as a Warn signal, never Critical, so a repetitive
    // I/O agent (e.g. researcher fetching many URLs) is not gated on
    // repetition alone. Tick past the warm-up (min_turns = 3) first.
    let fp = 42u64;
    for turn in 1u64..=4 {
        let _ = mon.tick(
            turn,
            &[ToolObservation { fingerprint: fp, failed: false }],
            None,
            &quiet_guard_state(),
        );
    }
    let r = mon.tick(
        5,
        &[ToolObservation { fingerprint: fp, failed: false }],
        None,
        &quiet_guard_state(),
    );

    let entropy_signals: Vec<_> = r
        .health
        .signals()
        .iter()
        .filter(|s| s.kind == DivergenceSignalKind::RepetitionEntropy)
        .collect();
    assert!(
        !entropy_signals.is_empty(),
        "expected an entropy advisory signal from repetition"
    );
    assert!(
        entropy_signals
            .iter()
            .all(|s| s.severity == SignalSeverity::Warn),
        "repetition entropy must be advisory (Warn), never Critical: {entropy_signals:?}"
    );
    // Entropy alone must not drive the session to Critical (the operator gate).
    assert!(
        !matches!(r.health, TrajectoryHealth::Critical { .. }),
        "entropy alone must not reach Critical: {:?}",
        r.health
    );

    // Payload must still carry signals (advisory divergence is recorded).
    let payload = build_event_payload(&r.health).unwrap();
    assert!(payload["signals"].as_array().map(|a| a.len() > 0).unwrap_or(false));
}
