//! Trajectory Health — divergence-signal substrate (P0).
//!
//! This module defines the data shape consumed by the in-gateway divergence
//! monitor (P1) and the optional LLM watchdog (P3). It is intentionally
//! **pure data + pure logic**: no IO, no thresholds wired into production
//! turn paths, no consumer reads `TrajectoryHealth` yet.
//!
//! Layers (see `docs/design/divergence-sentinel-design.md` §4):
//!
//! - **Signals** are individual observations (`DivergenceSignalKind`) with a
//!   numeric `current` value vs a `threshold`, tagged with a `severity`
//!   (`Warn` near a trip, `Critical` very near a trip).
//! - **Aggregation** rolls a list of signals up into a `TrajectoryHealth`
//!   level (`Healthy` / `Watching` / `Diverging` / `Critical`).
//! - **Causal-event payload** turns a `TrajectoryHealth` into a JSON object
//!   ready for `SessionTracer::log_event` with category
//!   [`DIVERGENCE_CATEGORY`] and one of the [`DIVERGENCE_ACTION_*`] actions.
//!
//! P1 will plug real signal extractors into the turn loop. P0 only ships
//! the shape so other crates can serialize/deserialize against a stable
//! schema.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::runtime::guard::LoopGuardState;

/// Causal-chain category used by every divergence event.
pub const DIVERGENCE_CATEGORY: &str = "divergence";

/// Emitted when at least one signal crossed a warn threshold but the
/// trajectory has not yet reached the action-required band.
pub const DIVERGENCE_ACTION_OBSERVED: &str = "observed";

/// Emitted when the trajectory is in the action-required band: planner
/// messaging (P2) and downstream consumers should react.
pub const DIVERGENCE_ACTION_DETECTED: &str = "detected";

/// Emitted when the trajectory is approaching a hard trip (~95%+ on at
/// least one axis): operator notification (P2) should fire.
pub const DIVERGENCE_ACTION_ESCALATED: &str = "escalated";

/// Default warn fraction (80%) used to derive a `Warn` severity from
/// LoopGuard pressure values. Matches the existing
/// `LoopGuard::is_sub_trip_warning` threshold so the new substrate stays
/// consistent with P-7.18 degraded-mode entry.
pub const DEFAULT_WARN_THRESHOLD: f32 = 0.80;

/// Default critical fraction (95%) used to derive a `Critical` severity.
/// Above this, a trip is imminent and the operator should be notified.
pub const DEFAULT_CRITICAL_THRESHOLD: f32 = 0.95;

/// The individual divergence signals the monitor can compute.
///
/// New variants land in P1 as additional signal extractors are wired in.
/// The set here is the minimum substrate required for aggregation logic
/// to be testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceSignalKind {
    /// `current_loops / max_loops_without_progress` from LoopGuard.
    LoopPressure,
    /// `max(tool_failures) / max_tool_failures` from LoopGuard.
    FailurePressure,
    /// `child_failures / max_child_failures` from LoopGuard.
    ChildFailurePressure,
    /// Turns since the last new causal-event `category` was observed.
    /// Computed by the monitor (P1) from event history.
    DigestStall,
    /// Shannon entropy of the last-N tool fingerprints. Low entropy means
    /// the agent is repeating itself. Computed by the monitor (P1).
    RepetitionEntropy,
    /// Number of error events in the last-N turns. Computed by the
    /// monitor (P1).
    ErrorBurst,
    /// Prompt-context utilization fraction. Already emitted today by
    /// `budget_tracker::emit_context_pressure_high_if_warranted` — the
    /// monitor (P1) will subscribe to that signal here too.
    ContextPressure,
}

impl DivergenceSignalKind {
    /// Stable string slug for serialization into causal-event payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LoopPressure => "loop_pressure",
            Self::FailurePressure => "failure_pressure",
            Self::ChildFailurePressure => "child_failure_pressure",
            Self::DigestStall => "digest_stall",
            Self::RepetitionEntropy => "repetition_entropy",
            Self::ErrorBurst => "error_burst",
            Self::ContextPressure => "context_pressure",
        }
    }
}

/// Severity assigned to a single signal.
///
/// `Warn` means the signal is in the warn band (≥ warn threshold, < critical
/// threshold). `Critical` means it is in the critical band (≥ critical
/// threshold) — a trip is imminent. Severity is per-signal, not per-session;
/// session-level health is derived by aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSeverity {
    Warn,
    Critical,
}

impl SignalSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

/// One concrete observation of divergence on a single axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DivergenceSignal {
    pub kind: DivergenceSignalKind,
    pub severity: SignalSeverity,
    /// The measured value on this axis. Conventionally a fraction in
    /// `[0.0, 1.0+]` for pressure-style signals; raw count for
    /// `DigestStall` / `ErrorBurst`; bits for `RepetitionEntropy`.
    pub current: f32,
    /// The threshold this signal crossed to earn its severity (warn or
    /// critical). Stored so consumers can recompute "how far past the
    /// line" without re-deriving the threshold table.
    pub threshold: f32,
    /// Optional short evidence string (e.g. "tool 'web.search' failed 4/5
    /// times"). Free-form; intended for downstream display, not parsing.
    #[serde(default)]
    pub evidence: Option<String>,
}

impl DivergenceSignal {
    pub fn new(kind: DivergenceSignalKind, severity: SignalSeverity, current: f32, threshold: f32) -> Self {
        Self {
            kind,
            severity,
            current,
            threshold,
            evidence: None,
        }
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = Some(evidence.into());
        self
    }
}

/// Session-level divergence verdict, aggregated from a list of signals.
///
/// `Healthy` means no signal warranted attention. `Watching` is below the
/// action band — surface it in the causal chain so operators can audit, but
/// do not message the planner yet. `Diverging` is the action band — planner
/// notification (P2) fires here. `Critical` is the imminent-trip band —
/// operator notification (P2) fires here in addition to planner messaging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "level", rename_all = "snake_case")]
pub enum TrajectoryHealth {
    Healthy,
    Watching { signals: Vec<DivergenceSignal> },
    Diverging { signals: Vec<DivergenceSignal> },
    Critical { signals: Vec<DivergenceSignal> },
}

impl TrajectoryHealth {
    /// Stable string slug for the level — convenient for logging / metrics
    /// labels without round-tripping through serde.
    pub fn level_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Watching { .. } => "watching",
            Self::Diverging { .. } => "diverging",
            Self::Critical { .. } => "critical",
        }
    }

    /// The causal-chain action slug associated with this level. Returns
    /// `None` for `Healthy` because healthy sessions should not emit
    /// divergence events at all (we do not pollute the causal chain with
    /// "everything is fine" entries).
    pub fn causal_action(&self) -> Option<&'static str> {
        match self {
            Self::Healthy => None,
            Self::Watching { .. } => Some(DIVERGENCE_ACTION_OBSERVED),
            Self::Diverging { .. } => Some(DIVERGENCE_ACTION_DETECTED),
            Self::Critical { .. } => Some(DIVERGENCE_ACTION_ESCALATED),
        }
    }

    /// Returns the signals carried with this verdict, or an empty slice for
    /// `Healthy`. Avoids forcing every caller through a match.
    pub fn signals(&self) -> &[DivergenceSignal] {
        match self {
            Self::Healthy => &[],
            Self::Watching { signals } | Self::Diverging { signals } | Self::Critical { signals } => signals,
        }
    }
}

/// Aggregate a flat list of signals into a session-level verdict.
///
/// Decision rule (locked in by tests below — change with care):
///
/// 1. Any signal with `Critical` severity → [`TrajectoryHealth::Critical`].
/// 2. Else if ≥ 2 `Warn` signals → [`TrajectoryHealth::Diverging`].
/// 3. Else if ≥ 1 `Warn` signal → [`TrajectoryHealth::Watching`].
/// 4. Else → [`TrajectoryHealth::Healthy`].
///
/// The rule is intentionally simple: P0 ships a substrate, not a tuned
/// policy. Threshold tuning and signal-weighting land in P1 with real
/// session data to calibrate against.
pub fn aggregate(signals: Vec<DivergenceSignal>) -> TrajectoryHealth {
    if signals.is_empty() {
        return TrajectoryHealth::Healthy;
    }

    let critical_count = signals.iter().filter(|s| s.severity == SignalSeverity::Critical).count();
    if critical_count > 0 {
        return TrajectoryHealth::Critical { signals };
    }

    let warn_count = signals.iter().filter(|s| s.severity == SignalSeverity::Warn).count();
    if warn_count >= 2 {
        return TrajectoryHealth::Diverging { signals };
    }
    if warn_count == 1 {
        return TrajectoryHealth::Watching { signals };
    }

    TrajectoryHealth::Healthy
}

/// Classify a pressure value `[0.0, 1.0+]` against the default warn/critical
/// thresholds, returning a signal of the given `kind` if either threshold is
/// crossed. Returns `None` when the value is below the warn threshold.
///
/// "Pressure" signals are fractions of an existing LoopGuard limit. For
/// non-pressure signals (digest stall in turns, error burst counts) callers
/// should construct `DivergenceSignal` directly with a domain-appropriate
/// threshold.
pub fn classify_pressure(kind: DivergenceSignalKind, current: f32) -> Option<DivergenceSignal> {
    if current >= DEFAULT_CRITICAL_THRESHOLD {
        Some(DivergenceSignal::new(
            kind,
            SignalSeverity::Critical,
            current,
            DEFAULT_CRITICAL_THRESHOLD,
        ))
    } else if current >= DEFAULT_WARN_THRESHOLD {
        Some(DivergenceSignal::new(
            kind,
            SignalSeverity::Warn,
            current,
            DEFAULT_WARN_THRESHOLD,
        ))
    } else {
        None
    }
}

/// Derive the LoopGuard-shaped signals from a snapshot of `LoopGuard`.
///
/// This is the bridge between the existing `LoopGuard::is_sub_trip_warning`
/// behavior (a single bool) and the richer multi-signal view the monitor
/// consumes. The bool stays where it is for P-7.18 degraded-mode entry;
/// this function is additive.
///
/// Signals returned:
/// - [`DivergenceSignalKind::LoopPressure`] when
///   `current_loops / max_loops_without_progress` ≥ warn threshold.
/// - [`DivergenceSignalKind::FailurePressure`] when any single tool's
///   `failures / max_tool_failures` ≥ warn threshold. Evidence names the
///   worst offender tool and its count.
/// - [`DivergenceSignalKind::ChildFailurePressure`] when
///   `child_failures / max_child_failures` ≥ warn threshold.
///
/// Returns an empty vec when nothing crosses the warn threshold — that
/// translates to `TrajectoryHealth::Healthy` after [`aggregate`].
pub fn signals_from_loop_guard(state: &LoopGuardState) -> Vec<DivergenceSignal> {
    let mut signals = Vec::new();

    if let Some(s) = loop_pressure_signal(state) {
        signals.push(s);
    }
    if let Some(s) = failure_pressure_signal(state) {
        signals.push(s);
    }
    if let Some(s) = child_failure_pressure_signal(state) {
        signals.push(s);
    }

    signals
}

fn loop_pressure_signal(state: &LoopGuardState) -> Option<DivergenceSignal> {
    if state.max_loops_without_progress == 0 {
        return None;
    }
    let current = state.current_loops as f32 / state.max_loops_without_progress as f32;
    classify_pressure(DivergenceSignalKind::LoopPressure, current).map(|s| {
        s.with_evidence(format!(
            "{} consecutive cycles without meaningful progress (limit {})",
            state.current_loops, state.max_loops_without_progress
        ))
    })
}

fn failure_pressure_signal(state: &LoopGuardState) -> Option<DivergenceSignal> {
    if state.max_tool_failures == 0 {
        return None;
    }
    let (worst_tool, worst_count) = match worst_tool_failure(&state.tool_failure_counts) {
        Some(pair) => pair,
        None => return None,
    };
    let current = worst_count as f32 / state.max_tool_failures as f32;
    classify_pressure(DivergenceSignalKind::FailurePressure, current).map(|s| {
        s.with_evidence(format!(
            "tool '{}' failed {} times this session (limit {})",
            worst_tool, worst_count, state.max_tool_failures
        ))
    })
}

fn child_failure_pressure_signal(state: &LoopGuardState) -> Option<DivergenceSignal> {
    if state.max_child_failures == 0 {
        return None;
    }
    let current = state.child_failure_count as f32 / state.max_child_failures as f32;
    classify_pressure(DivergenceSignalKind::ChildFailurePressure, current).map(|s| {
        s.with_evidence(format!(
            "{} child agent tasks have failed (limit {})",
            state.child_failure_count, state.max_child_failures
        ))
    })
}

/// Pick the tool with the highest failure count. Ties are broken by
/// alphabetically smaller tool name so the result is deterministic
/// across runs — `HashMap` iteration order is randomized and would
/// otherwise produce flaky evidence strings and non-reproducible
/// causal-chain payloads.
fn worst_tool_failure(counts: &HashMap<String, u32>) -> Option<(String, u32)> {
    counts
        .iter()
        .max_by(|(name_a, count_a), (name_b, count_b)| {
            // Primary: higher count wins.
            // Tie-break: alphabetically smaller name wins. `max_by` picks
            // the element for which the comparator returns `Greater`, so
            // when counts are equal we need to flip the name ordering.
            count_a
                .cmp(count_b)
                .then_with(|| name_b.cmp(name_a))
        })
        .map(|(name, count)| (name.clone(), *count))
}

/// Build the JSON payload a `SessionTracer::log_event` call should attach
/// to a divergence event.
///
/// The payload shape is stable across all `divergence.*` actions:
///
/// ```json
/// {
///   "level": "watching" | "diverging" | "critical",
///   "signals": [
///     {
///       "kind": "loop_pressure",
///       "severity": "warn",
///       "current": 0.82,
///       "threshold": 0.80,
///       "evidence": "…"
///     },
///     …
///   ]
/// }
/// ```
///
/// Returns `None` when `health` is `Healthy` — healthy sessions should
/// not emit divergence events.
pub fn build_event_payload(health: &TrajectoryHealth) -> Option<serde_json::Value> {
    health.causal_action()?;
    Some(serde_json::json!({
        "level": health.level_str(),
        "signals": health.signals(),
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn warn(kind: DivergenceSignalKind) -> DivergenceSignal {
        DivergenceSignal::new(kind, SignalSeverity::Warn, 0.85, DEFAULT_WARN_THRESHOLD)
    }

    fn critical(kind: DivergenceSignalKind) -> DivergenceSignal {
        DivergenceSignal::new(kind, SignalSeverity::Critical, 0.98, DEFAULT_CRITICAL_THRESHOLD)
    }

    // ── Aggregation rule ────────────────────────────────────────────────

    #[test]
    fn aggregate_empty_is_healthy() {
        assert!(matches!(aggregate(vec![]), TrajectoryHealth::Healthy));
    }

    #[test]
    fn aggregate_one_warn_is_watching() {
        let out = aggregate(vec![warn(DivergenceSignalKind::LoopPressure)]);
        assert!(matches!(out, TrajectoryHealth::Watching { .. }));
    }

    #[test]
    fn aggregate_two_warns_is_diverging() {
        let out = aggregate(vec![
            warn(DivergenceSignalKind::LoopPressure),
            warn(DivergenceSignalKind::FailurePressure),
        ]);
        assert!(matches!(out, TrajectoryHealth::Diverging { .. }));
    }

    #[test]
    fn aggregate_any_critical_is_critical() {
        // Critical wins even when accompanied by warns.
        let out = aggregate(vec![
            warn(DivergenceSignalKind::LoopPressure),
            critical(DivergenceSignalKind::FailurePressure),
        ]);
        assert!(matches!(out, TrajectoryHealth::Critical { .. }));
    }

    #[test]
    fn aggregate_lone_critical_is_critical() {
        let out = aggregate(vec![critical(DivergenceSignalKind::LoopPressure)]);
        assert!(matches!(out, TrajectoryHealth::Critical { .. }));
    }

    // ── Pressure classification ─────────────────────────────────────────

    #[test]
    fn classify_below_warn_returns_none() {
        assert!(classify_pressure(DivergenceSignalKind::LoopPressure, 0.5).is_none());
        assert!(classify_pressure(DivergenceSignalKind::LoopPressure, DEFAULT_WARN_THRESHOLD - 0.01).is_none());
    }

    #[test]
    fn classify_at_warn_threshold_is_warn() {
        let s = classify_pressure(DivergenceSignalKind::LoopPressure, DEFAULT_WARN_THRESHOLD).unwrap();
        assert_eq!(s.severity, SignalSeverity::Warn);
        assert_eq!(s.threshold, DEFAULT_WARN_THRESHOLD);
    }

    #[test]
    fn classify_at_critical_threshold_is_critical() {
        let s = classify_pressure(DivergenceSignalKind::LoopPressure, DEFAULT_CRITICAL_THRESHOLD).unwrap();
        assert_eq!(s.severity, SignalSeverity::Critical);
        assert_eq!(s.threshold, DEFAULT_CRITICAL_THRESHOLD);
    }

    #[test]
    fn classify_above_critical_stays_critical() {
        let s = classify_pressure(DivergenceSignalKind::LoopPressure, 1.5).unwrap();
        assert_eq!(s.severity, SignalSeverity::Critical);
    }

    // ── LoopGuard bridge ────────────────────────────────────────────────

    fn state_with(loops: u32, max_loops: u32, child_failures: u32, max_children: u32) -> LoopGuardState {
        LoopGuardState {
            max_loops_without_progress: max_loops,
            max_child_failures: max_children,
            current_loops: loops,
            child_failure_count: child_failures,
            ..Default::default()
        }
    }

    #[test]
    fn loop_guard_quiet_state_yields_no_signals() {
        let state = state_with(0, 5, 0, 3);
        assert!(signals_from_loop_guard(&state).is_empty());
    }

    #[test]
    fn loop_guard_warn_loops_pressure_surfaces() {
        // 4/5 = 0.80 → warn threshold.
        let state = state_with(4, 5, 0, 3);
        let signals = signals_from_loop_guard(&state);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, DivergenceSignalKind::LoopPressure);
        assert_eq!(signals[0].severity, SignalSeverity::Warn);
        assert!(signals[0].evidence.as_deref().unwrap().contains("4 consecutive"));
    }

    #[test]
    fn loop_guard_critical_loops_pressure_surfaces() {
        // 5/5 = 1.0 → critical threshold (≥ 0.95).
        let state = state_with(5, 5, 0, 3);
        let signals = signals_from_loop_guard(&state);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, DivergenceSignalKind::LoopPressure);
        assert_eq!(signals[0].severity, SignalSeverity::Critical);
    }

    #[test]
    fn loop_guard_picks_worst_failing_tool_for_evidence() {
        let mut state = state_with(0, 10, 0, 5);
        state.tool_failure_counts.insert("web.search".into(), 3);
        state.tool_failure_counts.insert("sandbox.exec".into(), 7); // 7/8 = 0.875 → warn
        let signals = signals_from_loop_guard(&state);
        assert_eq!(signals.len(), 1);
        let s = &signals[0];
        assert_eq!(s.kind, DivergenceSignalKind::FailurePressure);
        assert!(s.evidence.as_deref().unwrap().contains("sandbox.exec"));
        assert!(s.evidence.as_deref().unwrap().contains("7 times"));
    }

    #[test]
    fn worst_tool_failure_tie_breaks_on_alphabetical_name() {
        // HashMap iteration is randomized. Without a deterministic tie-break,
        // ties on count would flip the chosen "worst" tool across runs and
        // make evidence strings (and replayed causal events) non-reproducible.
        // We pin: when counts are equal, the alphabetically smaller name wins.
        // Repeat the assertion many times — at least one shuffle would
        // surface non-determinism if it reappeared.
        for _ in 0..64 {
            let mut counts: HashMap<String, u32> = HashMap::new();
            counts.insert("zebra".into(), 4);
            counts.insert("alpha".into(), 4);
            counts.insert("mango".into(), 4);
            let (name, count) = worst_tool_failure(&counts).unwrap();
            assert_eq!(name, "alpha", "tie-break must pick alphabetically smallest");
            assert_eq!(count, 4);
        }
    }

    #[test]
    fn worst_tool_failure_higher_count_beats_alphabetical_order() {
        // Tie-break only kicks in on equal counts — a higher count must
        // still win even when its name is alphabetically larger.
        let mut counts: HashMap<String, u32> = HashMap::new();
        counts.insert("alpha".into(), 2);
        counts.insert("zebra".into(), 5);
        let (name, count) = worst_tool_failure(&counts).unwrap();
        assert_eq!(name, "zebra");
        assert_eq!(count, 5);
    }

    #[test]
    fn loop_guard_child_failure_pressure_surfaces() {
        // 3/3 → critical.
        let state = state_with(0, 5, 3, 3);
        let signals = signals_from_loop_guard(&state);
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].kind, DivergenceSignalKind::ChildFailurePressure);
        assert_eq!(signals[0].severity, SignalSeverity::Critical);
    }

    #[test]
    fn loop_guard_zero_limits_emit_nothing() {
        // Guards against div-by-zero when a limit is misconfigured to 0.
        let state = state_with(0, 0, 0, 0);
        assert!(signals_from_loop_guard(&state).is_empty());
    }

    #[test]
    fn loop_guard_multiple_pressures_aggregate_to_diverging() {
        let mut state = state_with(8, 10, 0, 5); // loop pressure: 8/10 = 0.80 → warn
        state.tool_failure_counts.insert("sandbox.exec".into(), 7); // failure pressure: 7/8 = 0.875 → warn
        let health = aggregate(signals_from_loop_guard(&state));
        assert!(matches!(health, TrajectoryHealth::Diverging { signals } if signals.len() == 2));
    }

    // ── Payload + slug stability ────────────────────────────────────────

    #[test]
    fn level_str_slugs_are_stable() {
        assert_eq!(TrajectoryHealth::Healthy.level_str(), "healthy");
        assert_eq!(
            TrajectoryHealth::Watching { signals: vec![] }.level_str(),
            "watching"
        );
        assert_eq!(
            TrajectoryHealth::Diverging { signals: vec![] }.level_str(),
            "diverging"
        );
        assert_eq!(
            TrajectoryHealth::Critical { signals: vec![] }.level_str(),
            "critical"
        );
    }

    #[test]
    fn causal_action_slugs_match_constants() {
        assert_eq!(TrajectoryHealth::Healthy.causal_action(), None);
        assert_eq!(
            TrajectoryHealth::Watching { signals: vec![] }.causal_action(),
            Some(DIVERGENCE_ACTION_OBSERVED)
        );
        assert_eq!(
            TrajectoryHealth::Diverging { signals: vec![] }.causal_action(),
            Some(DIVERGENCE_ACTION_DETECTED)
        );
        assert_eq!(
            TrajectoryHealth::Critical { signals: vec![] }.causal_action(),
            Some(DIVERGENCE_ACTION_ESCALATED)
        );
    }

    #[test]
    fn build_event_payload_returns_none_for_healthy() {
        assert!(build_event_payload(&TrajectoryHealth::Healthy).is_none());
    }

    #[test]
    fn build_event_payload_carries_level_and_signals() {
        let health = TrajectoryHealth::Diverging {
            signals: vec![warn(DivergenceSignalKind::LoopPressure)],
        };
        let payload = build_event_payload(&health).unwrap();
        assert_eq!(payload["level"], "diverging");
        let signals = payload["signals"].as_array().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0]["kind"], "loop_pressure");
        assert_eq!(signals[0]["severity"], "warn");
    }

    #[test]
    fn signal_kind_str_round_trips() {
        // The string slugs must stay stable — downstream consumers will
        // index causal-event payloads by these keys.
        for kind in [
            DivergenceSignalKind::LoopPressure,
            DivergenceSignalKind::FailurePressure,
            DivergenceSignalKind::ChildFailurePressure,
            DivergenceSignalKind::DigestStall,
            DivergenceSignalKind::RepetitionEntropy,
            DivergenceSignalKind::ErrorBurst,
            DivergenceSignalKind::ContextPressure,
        ] {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json.as_str().unwrap(), kind.as_str());
        }
    }
}
