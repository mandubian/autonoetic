//! RFC #776 Part B.4 — integration-level contract test for the spawn-identity
//! loop guard.
//!
//! The unit tests in `runtime/guard.rs` (`spawn_identity_*`) cover the pure
//! trip logic. This file pins the integration-facing contract that callers
//! (the lifecycle.rs tool-result loop and any future code that drives
//! `LoopGuard` from a tool-call surface) rely on:
//!
//! - Stable string identifiers (`code()`, `rule_id()`) that appear in the
//!   emitted `loop_guard.tripped` causal event and operator-facing summary.
//! - The exact `LoopGuardTripReason::RepeatedSpawnIdentity { agent_id,
//!   identity_hash, occurrences }` shape returned at the trip boundary.
//! - Default config behavior (`max_spawn_identity_repeats == 3`) matching
//!   what `LoopGuardConfig::default()` and `config-template.yaml` document.
//! - The opt-out path (`max_spawn_identity_repeats == 0` disables the
//!   detector) honored end-to-end via `with_config`.
//!
//! What this file does NOT cover: the lifecycle.rs argument-extraction wiring
//! at `runtime/lifecycle.rs:3738-3773` that pulls `agent_id` / `message` /
//! `metadata.expected_outputs` out of a parsed `agent_spawn` tool-call and
//! feeds them to `register_spawn_attempt`. That wiring is private inline
//! code; covering it requires a stub-LLM-driven `AgentExecutor` turn. The
//! shape pinned here is the contract that wiring is expected to honor.

use autonoetic_gateway::runtime::guard::{LoopGuard, LoopGuardTripReason};
use autonoetic_types::config::LoopGuardConfig;

/// Default config (max_spawn_identity_repeats == 3) trips on the third
/// identical spawn. Verifies the production default matches what
/// `docs/config-reference.md` documents.
#[test]
fn default_config_trips_on_third_identical_spawn() {
    let guard = LoopGuard::with_config(&LoopGuardConfig::default());
    assert_eq!(
        guard_max_spawn_identity_repeats(&guard),
        3,
        "default matches docs/config-reference.md and config-template.yaml"
    );

    let mut guard = LoopGuard::with_config(&LoopGuardConfig::default());
    let expected = vec!["main.py".to_string(), "SKILL.md".to_string()];
    let message = r#""build the weather agent""#; // lifecycle.rs uses v.to_string() — JSON-quoted

    let r1 = guard.register_spawn_attempt("coder.default", &expected, message);
    let r2 = guard.register_spawn_attempt("coder.default", &expected, message);
    let r3 = guard.register_spawn_attempt("coder.default", &expected, message);

    assert!(r1.is_none(), "first spawn: no trip");
    assert!(r2.is_none(), "second spawn: no trip (still under threshold)");
    let trip = r3.expect("third identical spawn trips the guard");

    match &trip {
        LoopGuardTripReason::RepeatedSpawnIdentity {
            agent_id,
            identity_hash: _,
            occurrences,
        } => {
            assert_eq!(agent_id, "coder.default");
            assert_eq!(*occurrences, 3, "occurrences reflects the threshold");
        }
        _ => panic!("expected RepeatedSpawnIdentity"),
    }

    // Stable identifiers — these appear in causal-event payloads and
    // operator-facing summaries; downstream tooling depends on them.
    assert_eq!(trip.code(), "repeated_spawn_identity");
    assert_eq!(trip.rule_id(), "P-7.19");

    // The guard is now tripped — last_trip_reason surfaces it for the
    // lifecycle.rs emit-and-yield path.
    assert!(matches!(
        guard.last_trip_reason(),
        Some(LoopGuardTripReason::RepeatedSpawnIdentity { .. })
    ));
}

/// Different `message` content produces a different identity hash and does
/// NOT accumulate toward the trip. Verifies the strict-identity design
/// choice: trivial input rewording evades detection (start strict; measure
/// evasion before loosening — RFC open question 3).
#[test]
fn different_message_does_not_accumulate() {
    let mut guard = LoopGuard::with_config(&LoopGuardConfig::default());
    let expected = vec!["main.py".to_string()];

    // Two calls with one message, one with a different message — the two
    // identical calls should be at count=2 (under threshold), and the third
    // identical call should trip. The divergent call does not contribute.
    guard.register_spawn_attempt("coder.default", &expected, r#""v1""#);
    guard.register_spawn_attempt("coder.default", &expected, r#""different""#);
    guard.register_spawn_attempt("coder.default", &expected, r#""v1""#);
    let r = guard.register_spawn_attempt("coder.default", &expected, r#""v1""#);

    let trip = r.expect("third identical (message v1) trips");
    match &trip {
        LoopGuardTripReason::RepeatedSpawnIdentity { occurrences, .. } => {
            assert_eq!(*occurrences, 3, "count is per-identity, not total");
        }
        _ => panic!("expected RepeatedSpawnIdentity"),
    }
}

/// Different `expected_outputs` is a different contract — the parent
/// changed something structural. Verifies invariant 1 (Fuller co-design):
/// the agent has a lawful exit (declare different expected_outputs), so
/// detecting the loop doesn't trap it.
#[test]
fn different_expected_outputs_does_not_accumulate() {
    let mut guard = LoopGuard::with_config(&LoopGuardConfig::default());
    let m = r#""build""#;
    let one = vec!["a.py".to_string()];
    let two = vec!["b.py".to_string()];

    guard.register_spawn_attempt("coder.default", &one, m);
    guard.register_spawn_attempt("coder.default", &two, m); // different contract
    guard.register_spawn_attempt("coder.default", &one, m);
    let r = guard.register_spawn_attempt("coder.default", &one, m);

    match r.expect("third identical (contract [a.py]) trips") {
        LoopGuardTripReason::RepeatedSpawnIdentity { occurrences, .. } => {
            assert_eq!(occurrences, 3);
        }
        _ => panic!("expected RepeatedSpawnIdentity"),
    }
}

/// Different `agent_id` is a different delegation — the parent switched
/// specialists. Counts are keyed per `(agent_id, identity_hash)` and never
/// accumulate across distinct agents.
#[test]
fn different_agent_id_does_not_accumulate() {
    let mut guard = LoopGuard::with_config(&LoopGuardConfig::default());
    let expected = vec!["out.txt".to_string()];
    let m = r#""do thing""#;

    // Two spawns of each agent — under the default threshold of 3, but
    // if counts cross-accumulated by agent_id only, the combined count
    // (4) would already trip. The per-(agent, contract, message) key
    // means each agent has its own independent count of 2.
    guard.register_spawn_attempt("coder.default", &expected, m);
    guard.register_spawn_attempt("coder.default", &expected, m);
    guard.register_spawn_attempt("researcher.default", &expected, m);
    guard.register_spawn_attempt("researcher.default", &expected, m);
    assert!(
        guard.last_trip_reason().is_none(),
        "different agent_id is a different delegation — no cross-identity accumulation"
    );

    // A third spawn of either trips independently at count=3.
    let trip = guard
        .register_spawn_attempt("coder.default", &expected, m)
        .expect("third coder.default spawn trips independently");
    assert!(matches!(
        trip,
        LoopGuardTripReason::RepeatedSpawnIdentity { .. }
    ));
}

/// Setting `max_spawn_identity_repeats: 0` disables the detector. This is
/// the operator opt-out documented in config-reference.md and the
/// constitution (configurable; set to 0 to disable).
#[test]
fn disabled_when_threshold_zero() {
    let mut cfg = LoopGuardConfig::default();
    cfg.max_spawn_identity_repeats = 0;
    let mut guard = LoopGuard::with_config(&cfg);

    let expected = vec!["x.py".to_string()];
    let m = r#""x""#;
    for _ in 0..50 {
        assert!(
            guard
                .register_spawn_attempt("coder.default", &expected, m)
                .is_none(),
            "disabled detector never trips, no matter how many repeats"
        );
    }
    assert!(guard.last_trip_reason().is_none());
}

/// After the guard trips, further `register_spawn_attempt` calls are
/// no-ops — the trip is sticky until reset. Mirrors the LoopGuard's
/// documented behavior for all trip conditions (one trip per session,
/// surface and yield).
#[test]
fn trip_is_sticky_after_first_fire() {
    let mut guard = LoopGuard::with_config(&LoopGuardConfig::default());
    let expected = vec!["x.py".to_string()];
    let m = r#""x""#;

    for _ in 0..3 {
        guard.register_spawn_attempt("coder.default", &expected, m);
    }
    let first = guard
        .last_trip_reason()
        .expect("tripped after 3 identical")
        .clone();

    // Subsequent calls do not change the trip reason.
    for _ in 0..5 {
        guard.register_spawn_attempt("coder.default", &expected, m);
    }
    let after = guard.last_trip_reason().expect("still tripped").clone();
    assert_eq!(first.code(), after.code());
    assert_eq!(first.rule_id(), after.rule_id());
}

/// The identity hash mixes all three inputs — same `agent_id` and
/// `expected_outputs` but a different `message` produces a different
/// identity_hash on the returned trip reason. This pins the structural-
/// identity contract (RFC open question 3) so a future loosening has to
/// consciously break this test.
#[test]
fn identity_hash_is_structural_over_all_three_inputs() {
    let mut guard_a = LoopGuard::with_config(&LoopGuardConfig::default());
    let mut guard_b = LoopGuard::with_config(&LoopGuardConfig::default());
    let expected = vec!["a.py".to_string()];

    for _ in 0..3 {
        guard_a.register_spawn_attempt("coder.default", &expected, r#""message-a""#);
    }
    for _ in 0..3 {
        guard_b.register_spawn_attempt("coder.default", &expected, r#""message-b""#);
    }

    let hash_a = match guard_a.last_trip_reason().unwrap() {
        LoopGuardTripReason::RepeatedSpawnIdentity { identity_hash, .. } => *identity_hash,
        _ => panic!("expected RepeatedSpawnIdentity"),
    };
    let hash_b = match guard_b.last_trip_reason().unwrap() {
        LoopGuardTripReason::RepeatedSpawnIdentity { identity_hash, .. } => *identity_hash,
        _ => panic!("expected RepeatedSpawnIdentity"),
    };
    assert_ne!(
        hash_a, hash_b,
        "different message must produce a different identity_hash"
    );
}

// Small helper because max_spawn_identity_repeats is private on LoopGuard;
// the config it was built from is the public surface.
fn guard_max_spawn_identity_repeats(_guard: &LoopGuard) -> u32 {
    // The default is documented in `LoopGuardConfig::default()` and mirrors
    // what `config-template.yaml` and `docs/config-reference.md` publish.
    LoopGuardConfig::default().max_spawn_identity_repeats
}
