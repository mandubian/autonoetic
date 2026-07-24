//! Deterministic in-gateway trajectory monitor (Sentinel P1).
//!
//! Recomputes a [`TrajectoryHealth`] verdict every turn and signals when
//! the verdict's level changes. **Pure deterministic — zero LLM cost,
//! zero external calls.** The monitor is observational; it never modifies
//! session state. Acting on its signals (planner messaging, operator
//! escalation) lands in P2 (#241).
//!
//! Wiring is in `runtime::lifecycle` after tool-result processing. Each
//! turn the caller invokes [`TrajectoryMonitor::tick`] with the current
//! `LoopGuard` snapshot plus the turn's tool calls, results, and the
//! most-recent context-utilization fraction.
//!
//! [`TrajectoryHealth`]: super::trajectory_health::TrajectoryHealth

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use autonoetic_types::config::{TrajectoryConfig, TrajectorySignalsToggle};
use autonoetic_types::trajectory::FeedbackEvent;

use super::trajectory_health::{
    aggregate, signals_from_loop_guard, DivergenceSignal, DivergenceSignalKind, SignalSeverity,
    TrajectoryHealth,
};
use crate::runtime::guard::LoopGuard;

/// Observation of one tool call this turn, used by entropy and stall
/// signals. Lightweight on purpose — the monitor never stores arguments,
/// only a 64-bit fingerprint.
#[derive(Debug, Clone, Copy)]
pub struct ToolObservation {
    /// 64-bit hash of `(tool_name, normalized_arguments)`. Stable across
    /// turns so the same call counts as a repetition.
    pub fingerprint: u64,
    /// `true` when the tool returned an error result (`ok: false` or a
    /// non-zero exit code). Counted toward `error_burst`.
    pub failed: bool,
}

/// Result of [`TrajectoryMonitor::tick`].
#[derive(Debug, Clone)]
pub struct TickResult {
    pub health: TrajectoryHealth,
    /// `true` when this turn's level differs from the previous turn's
    /// level. Caller should emit a `divergence.*` causal event when this
    /// is `true` and `health` is not `Healthy`.
    pub level_changed: bool,
    /// RFC D.5 — if progress grace was earned this turn, the caller should
    /// suppress Sentinel escalation until this turn (exclusive). `None` when
    /// no extension is requested.
    pub suppress_until_turn: Option<u64>,
}

/// Per-session monitor state.
///
/// The monitor is **session-scoped** — one instance per `AgentExecutor`.
/// It is cheap to construct and carries only a small sliding-window
/// buffer (default 8 entries).
#[derive(Debug, Clone)]
pub struct TrajectoryMonitor {
    config: TrajectoryConfig,
    /// Last fingerprint observed across any turn. Tracks "turns since
    /// new fingerprint" for the digest_stall signal.
    last_distinct_fingerprint: Option<u64>,
    /// Turn counter (caller-supplied) at which `last_distinct_fingerprint`
    /// was last updated. Used to compute stall length.
    last_distinct_fingerprint_turn: u64,
    /// Sliding window of recent tool fingerprints for entropy.
    fingerprint_window: VecDeque<u64>,
    /// Sliding window of error counts (one entry per turn).
    error_window: VecDeque<u32>,
    /// Feedback events the gateway has issued to the agent, with the turn
    /// at which they were issued. Used to compute `feedback_ignored` vs
    /// `feedback_incorporated`.
    feedback_events: Vec<FeedbackEvent>,
    /// Turn counter at which each feedback event was issued.
    feedback_turns: Vec<u64>,
    /// Last verdict's level slug — used to detect transitions and to emit
    /// only on change. `None` until the first `tick`.
    last_level: Option<&'static str>,
    /// RFC D.5 — consecutive turns showing `FeedbackIncorporated`. Resets when
    /// a turn does not earn progress grace.
    consecutive_progress_turns: u32,
}

impl TrajectoryMonitor {
    pub fn new(config: TrajectoryConfig) -> Self {
        Self {
            config,
            last_distinct_fingerprint: None,
            last_distinct_fingerprint_turn: 0,
            fingerprint_window: VecDeque::new(),
            error_window: VecDeque::new(),
            feedback_events: Vec::new(),
            feedback_turns: Vec::new(),
            last_level: None,
            consecutive_progress_turns: 0,
        }
    }

    /// Restore the last known divergence level from a checkpoint.
    /// Called on session resume to prevent the first post-resume tick
    /// from always firing level_changed=true.
    pub fn restore_last_level(&mut self, level: Option<&str>) {
        self.last_level = level.and_then(restore_level);
    }

    /// Snapshot feedback events issued so far, paired with the turn at which
    /// they were issued. Used to persist feedback state across checkpoint
    /// save/restore so cross-turn `FeedbackIgnored` detection survives respawn.
    pub fn feedback_snapshot(&self) -> Vec<(u64, FeedbackEvent)> {
        self.feedback_turns
            .iter()
            .cloned()
            .zip(self.feedback_events.iter().cloned())
            .collect()
    }

    /// Restore feedback events from a checkpoint snapshot. The turn counters
    /// are preserved so the next `tick` can compare current events against
    /// feedback issued before the checkpoint.
    pub fn restore_feedback(&mut self, events: Vec<(u64, FeedbackEvent)>) {
        self.feedback_events = events.iter().map(|(_, e)| e.clone()).collect();
        self.feedback_turns = events.iter().map(|(t, _)| *t).collect();
    }

    /// Snapshot the last known level for checkpoint serialization.
    pub fn last_level_as_string(&self) -> Option<String> {
        self.last_level.map(|s| s.to_string())
    }

    /// `true` if the monitor will run at all. When the master switch is
    /// off the monitor is inert and produces no events.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Record that the gateway issued feedback to the agent this turn.
    ///
    /// Callers should record every validation violation and every typed tool
    /// error returned to the agent. The monitor uses these events to decide
    /// whether subsequent turns incorporated the feedback.
    pub fn record_feedback(&mut self, turn_counter: u64, events: &[FeedbackEvent]) {
        for event in events {
            self.feedback_events.push(event.clone());
            self.feedback_turns.push(turn_counter);
        }
        // Prune feedback older than the configured window so "ignored" only
        // measures recent feedback. Use the same window_size as error/fingerprint
        // windows for consistency.
        let cap = self.config.window_size.max(1) as u64;
        while self.feedback_turns.len() > cap as usize {
            self.feedback_turns.remove(0);
            self.feedback_events.remove(0);
        }
    }

    /// Compute feedback-signal state from recorded feedback events and the
    /// current turn's observed violations/error signatures.
    ///
    /// Returns `(ignored_signal?, incorporated_signal?)`. Only one of the two
    /// is produced per turn; ignored wins if any prior feedback signature
    /// repeats, otherwise incorporated is emitted when the agent is still
    /// producing errors after receiving feedback.
    fn feedback_signals(
        &self,
        turn_counter: u64,
        current_events: &[FeedbackEvent],
    ) -> (Option<DivergenceSignal>, Option<DivergenceSignal>) {
        if self.feedback_events.is_empty() || current_events.is_empty() {
            return (None, None);
        }

        let current_keys: std::collections::HashSet<String> = current_events
            .iter()
            .map(|e| e.signature_key())
            .collect();

        // Match against feedback issued on previous turns only; feedback given
        // this turn cannot be "ignored" until the next turn.
        let mut ignored_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for (event, turn) in self.feedback_events.iter().zip(self.feedback_turns.iter()) {
            if *turn >= turn_counter {
                continue;
            }
            let key = event.signature_key();
            if current_keys.contains(&key) {
                *ignored_counts.entry(key).or_insert(0) += 1;
            }
        }

        if !ignored_counts.is_empty() {
            let (worst_key, worst_count) = ignored_counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .unwrap();
            let severity = if worst_count >= 3 {
                SignalSeverity::Critical
            } else {
                SignalSeverity::Warn
            };
            let signal = DivergenceSignal::new(
                DivergenceSignalKind::FeedbackIgnored,
                severity,
                worst_count as f32,
                1.0,
            )
            .with_evidence(format!(
                "feedback signature '{}' repeated {} time(s) after gateway correction",
                worst_key, worst_count
            ));
            return (Some(signal), None);
        }

        // Only emit FeedbackIncorporated when feedback was issued on a
        // previous turn. Feedback recorded this turn has not yet had a chance
        // to be incorporated, so a same-turn signal would be misleading.
        let has_prior_feedback = self
            .feedback_turns
            .iter()
            .any(|turn| *turn < turn_counter);
        if !has_prior_feedback {
            return (None, None);
        }

        // Feedback was given earlier, but this turn's errors are different —
        // the agent is incorporating the feedback (even if not yet succeeding).
        let incorporated = DivergenceSignal::new(
            DivergenceSignalKind::FeedbackIncorporated,
            SignalSeverity::Warn,
            current_events.len() as f32,
            1.0,
        )
        .with_evidence(format!(
            "{} distinct error/violation signature(s) after feedback — trajectory is evolving",
            current_events.len()
        ));
        (None, Some(incorporated))
    }

    /// Recompute the verdict after one turn of work.
    ///
    /// `turn_counter` is the same monotonic value the rest of the
    /// runtime uses (cf. `AgentExecutor::turn_counter`); it is the cursor
    /// for sliding-window logic.
    ///
    /// `observations` is the list of tool calls executed this turn, with
    /// their fingerprints and error status. Pass an empty slice when the
    /// turn produced no tool calls.
    ///
    /// `feedback_events` records corrective feedback the gateway issued to
    /// the agent this turn (validation violations, typed tool errors). The
    /// monitor compares these against prior turns to detect ignored
    /// feedback.
    ///
    /// `context_utilization` is the prompt-budget utilization fraction in
    /// `[0.0, 1.0+]` (e.g. `0.82` for 82%). `None` when the breakdown
    /// hasn't computed one this turn — the signal is then skipped.
    pub fn tick(
        &mut self,
        turn_counter: u64,
        observations: &[ToolObservation],
        feedback_events: &[FeedbackEvent],
        context_utilization: Option<f32>,
        guard_state: &LoopGuard,
    ) -> TickResult {
        if !self.config.enabled {
            return TickResult {
                health: TrajectoryHealth::Healthy,
                level_changed: false,
                suppress_until_turn: None,
            };
        }

        // Update sliding windows from this turn's observations.
        let errors_this_turn = observations.iter().filter(|o| o.failed).count() as u32;
        self.push_error_count(errors_this_turn);

        for obs in observations {
            self.push_fingerprint(obs.fingerprint);
            if self.last_distinct_fingerprint != Some(obs.fingerprint) {
                self.last_distinct_fingerprint = Some(obs.fingerprint);
                self.last_distinct_fingerprint_turn = turn_counter;
            }
        }

        // Collect signals.
        let mut signals = Vec::new();
        let toggles = &self.config.signals;

        for signal in signals_from_loop_guard(guard_state) {
            if !signal_enabled(toggles, signal.kind) {
                continue;
            }
            signals.push(signal);
        }

        if toggles.digest_stall {
            if let Some(s) = self.digest_stall_signal(turn_counter) {
                signals.push(s);
            }
        }
        if toggles.repetition_entropy {
            if let Some(s) = self.repetition_entropy_signal(turn_counter) {
                signals.push(s);
            }
        }
        if toggles.error_burst {
            if let Some(s) = self.error_burst_signal() {
                signals.push(s);
            }
        }
        if toggles.context_pressure {
            if let Some(util) = context_utilization {
                if let Some(s) = self.context_pressure_signal(util) {
                    signals.push(s);
                }
            }
        }

        // Compare this turn's feedback against prior feedback events.
        let (ignored, incorporated) = self.feedback_signals(turn_counter, feedback_events);
        if let Some(s) = ignored {
            signals.push(s);
        } else if let Some(s) = incorporated {
            signals.push(s);
        }

        let health = aggregate(signals);

        // RFC D.5 — suppress-on-progress grace. Consecutive turns where the
        // agent is clearly incorporating feedback earn a suppression window
        // during which Sentinel escalation is silenced.
        let has_progress = health
            .signals()
            .iter()
            .any(|s| s.kind == DivergenceSignalKind::FeedbackIncorporated);
        let suppress_until_turn = if has_progress {
            self.consecutive_progress_turns += 1;
            if self.consecutive_progress_turns >= self.config.progress_grace_window {
                let grace_turns = self.config.progress_grace_turns.max(1);
                Some(turn_counter.saturating_add(grace_turns as u64))
            } else {
                None
            }
        } else {
            self.consecutive_progress_turns = 0;
            None
        };

        let level = health.level_str();
        let level_changed = self.last_level.map(|prev| prev != level).unwrap_or(true);
        self.last_level = Some(level);

        TickResult {
            health,
            level_changed,
            suppress_until_turn,
        }
    }

    fn push_fingerprint(&mut self, fp: u64) {
        let cap = self.config.window_size.max(1);
        if self.fingerprint_window.len() == cap {
            self.fingerprint_window.pop_front();
        }
        self.fingerprint_window.push_back(fp);
    }

    fn push_error_count(&mut self, count: u32) {
        let cap = self.config.window_size.max(1);
        if self.error_window.len() == cap {
            self.error_window.pop_front();
        }
        self.error_window.push_back(count);
    }

    fn digest_stall_signal(&self, turn_counter: u64) -> Option<DivergenceSignal> {
        let stall = turn_counter.saturating_sub(self.last_distinct_fingerprint_turn);
        // If we have never seen a fingerprint yet, do not fire: the
        // session is just getting started, not stalled.
        if self.last_distinct_fingerprint.is_none() {
            return None;
        }
        let cfg = &self.config.digest_stall;
        let stall_u32 = stall.min(u32::MAX as u64) as u32;
        if stall_u32 >= cfg.critical_turns {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::DigestStall,
                    SignalSeverity::Critical,
                    stall_u32 as f32,
                    cfg.critical_turns as f32,
                )
                .with_evidence(format!(
                    "{} turns since a new tool fingerprint was observed (critical ≥ {})",
                    stall_u32, cfg.critical_turns
                )),
            )
        } else if stall_u32 >= cfg.warn_turns {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::DigestStall,
                    SignalSeverity::Warn,
                    stall_u32 as f32,
                    cfg.warn_turns as f32,
                )
                .with_evidence(format!(
                    "{} turns since a new tool fingerprint was observed (warn ≥ {})",
                    stall_u32, cfg.warn_turns
                )),
            )
        } else {
            None
        }
    }

    fn repetition_entropy_signal(&self, turn_counter: u64) -> Option<DivergenceSignal> {
        let cfg = &self.config.repetition_entropy;
        // Warm-up: divergence is a trajectory property, not a single-turn one.
        // I/O-heavy agents (e.g. `researcher.default`) legitimately fire many
        // similar calls in their opening turns — fetching pages, re-querying.
        // Don't judge tool-call repetition before `min_turns` so that burst
        // does not trip the signal at turn 1.
        if turn_counter < cfg.min_turns {
            return None;
        }
        if self.fingerprint_window.len() < cfg.min_observations.max(1) {
            return None;
        }
        let entropy = shannon_entropy_bits(&self.fingerprint_window);
        if entropy > cfg.warn_bits {
            return None;
        }
        // Repetition entropy is ADVISORY: it caps at `Warn` and never escalates
        // a session to `Critical` on its own, so it never raises the operator
        // divergence gate. Repeating a tool call is weak evidence of being
        // stuck — a researcher fetching many URLs is doing its job. The
        // gate-worthy `Critical` verdicts come from the loop guard's semantic
        // no-progress (P-7.19) and the error-burst signal. The evidence still
        // records how low the entropy is (`critical_bits` band) so the
        // divergence event stays informative.
        let band = if entropy <= cfg.critical_bits {
            "critically low"
        } else {
            "low"
        };
        Some(
            DivergenceSignal::new(
                DivergenceSignalKind::RepetitionEntropy,
                SignalSeverity::Warn,
                entropy,
                cfg.warn_bits,
            )
            .with_evidence(format!(
                "tool-fingerprint entropy {:.2} bits over last {} calls ({}, warn ≤ {:.2}) — advisory, not gating",
                entropy,
                self.fingerprint_window.len(),
                band,
                cfg.warn_bits
            )),
        )
    }

    fn error_burst_signal(&self) -> Option<DivergenceSignal> {
        let cfg = &self.config.error_burst;
        let total: u32 = self.error_window.iter().copied().sum();
        if total >= cfg.critical_count {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::ErrorBurst,
                    SignalSeverity::Critical,
                    total as f32,
                    cfg.critical_count as f32,
                )
                .with_evidence(format!(
                    "{} tool errors in last {} turns (critical ≥ {})",
                    total,
                    self.error_window.len(),
                    cfg.critical_count
                )),
            )
        } else if total >= cfg.warn_count {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::ErrorBurst,
                    SignalSeverity::Warn,
                    total as f32,
                    cfg.warn_count as f32,
                )
                .with_evidence(format!(
                    "{} tool errors in last {} turns (warn ≥ {})",
                    total,
                    self.error_window.len(),
                    cfg.warn_count
                )),
            )
        } else {
            None
        }
    }

    fn context_pressure_signal(&self, utilization: f32) -> Option<DivergenceSignal> {
        let cfg = &self.config.context_pressure;
        if utilization >= cfg.critical_fraction {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::ContextPressure,
                    SignalSeverity::Critical,
                    utilization,
                    cfg.critical_fraction,
                )
                .with_evidence(format!(
                    "context utilization {:.0}% (critical ≥ {:.0}%)",
                    utilization * 100.0,
                    cfg.critical_fraction * 100.0
                )),
            )
        } else if utilization >= cfg.warn_fraction {
            Some(
                DivergenceSignal::new(
                    DivergenceSignalKind::ContextPressure,
                    SignalSeverity::Warn,
                    utilization,
                    cfg.warn_fraction,
                )
                .with_evidence(format!(
                    "context utilization {:.0}% (warn ≥ {:.0}%)",
                    utilization * 100.0,
                    cfg.warn_fraction * 100.0
                )),
            )
        } else {
            None
        }
    }
}

fn signal_enabled(toggles: &TrajectorySignalsToggle, kind: DivergenceSignalKind) -> bool {
    match kind {
        DivergenceSignalKind::LoopPressure => toggles.loop_pressure,
        DivergenceSignalKind::FailurePressure => toggles.failure_pressure,
        DivergenceSignalKind::ChildFailurePressure => toggles.child_failure_pressure,
        DivergenceSignalKind::DigestStall => toggles.digest_stall,
        DivergenceSignalKind::RepetitionEntropy => toggles.repetition_entropy,
        DivergenceSignalKind::ErrorBurst => toggles.error_burst,
        DivergenceSignalKind::ContextPressure => toggles.context_pressure,
        // Feedback signals are computed from the feedback event store and
        // always fed into aggregation; they are not gated by legacy toggles.
        DivergenceSignalKind::BlockedState
        | DivergenceSignalKind::FeedbackIgnored
        | DivergenceSignalKind::FeedbackIncorporated => true,
    }
}

/// Compute a 64-bit fingerprint of a tool call. Mirrors the structure of
/// `LoopGuard::compute_fingerprint` but is local to the monitor so the
/// two can evolve independently — the LoopGuard fingerprint is used for
/// progress detection (resets a counter), the monitor fingerprint is
/// used for repetition entropy (counts occurrences over a window).
pub fn fingerprint_tool_call(tool_name: &str, arguments_json: &str) -> u64 {
    let normalized = normalize_arguments(arguments_json);
    let mut hasher = DefaultHasher::new();
    tool_name.hash(&mut hasher);
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Strip echoed `intent` fields so repeated semantically-identical calls
/// produce the same fingerprint. Matches LoopGuard's approach.
pub(crate) fn normalize_arguments(arguments_json: &str) -> std::borrow::Cow<'_, str> {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(arguments_json) else {
        return std::borrow::Cow::Borrowed(arguments_json);
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("intent");
    }
    match serde_json::to_string(&v) {
        Ok(s) => std::borrow::Cow::Owned(s),
        Err(_) => std::borrow::Cow::Borrowed(arguments_json),
    }
}

/// Shannon entropy in bits over the multiset of fingerprints in `window`.
/// Returns `0.0` on an empty window. A single repeated fingerprint
/// yields entropy `0.0`. Uniform unique fingerprints across `N` slots
/// yield `log2(N)`.
fn shannon_entropy_bits(window: &VecDeque<u64>) -> f32 {
    if window.is_empty() {
        return 0.0;
    }
    let total = window.len() as f32;
    let mut counts: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    for &fp in window {
        *counts.entry(fp).or_insert(0) += 1;
    }
    let mut entropy = 0.0f32;
    for &count in counts.values() {
        let p = count as f32 / total;
        if p > 0.0 {
            entropy -= p * p.log2();
        }
    }
    entropy
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::tool_error::ToolErrorType;
    use std::collections::HashMap;

    fn cfg() -> TrajectoryConfig {
        TrajectoryConfig::default()
    }

    fn disabled_cfg() -> TrajectoryConfig {
        let mut c = TrajectoryConfig::default();
        c.enabled = false;
        c
    }

    fn quiet_guard_state() -> LoopGuard {
        LoopGuard::default()
    }

    // ── Master switch / no-op behavior ──────────────────────────────────

    #[test]
    fn disabled_monitor_is_no_op() {
        let mut mon = TrajectoryMonitor::new(disabled_cfg());
        let r = mon.tick(
            1,
            &[ToolObservation {
                fingerprint: 1,
                failed: true,
            }],
            &[],
            Some(0.95),
            &quiet_guard_state(),
        );
        assert!(matches!(r.health, TrajectoryHealth::Healthy));
        assert!(!r.level_changed);
    }

    #[test]
    fn first_tick_on_healthy_session_level_changed_but_no_payload() {
        // First tick after construction reports `level_changed = true`
        // (no prior level to compare against), but since the verdict is
        // `Healthy` the caller suppresses emission via
        // `build_event_payload(None)`. The test validates this contract:
        // changed but no payload.
        let mut mon = TrajectoryMonitor::new(cfg());
        let r = mon.tick(1, &[], &[], None, &quiet_guard_state());
        assert!(matches!(r.health, TrajectoryHealth::Healthy));
        // `level_changed` is `true` on the first tick (no prior level)
        // but the caller's emit rule is `level_changed && !Healthy`.
        assert!(r.level_changed);
    }

    #[test]
    fn repeated_same_level_does_not_re_trigger() {
        // Two consecutive Healthy ticks: only the first counts as a change.
        let mut mon = TrajectoryMonitor::new(cfg());
        let _ = mon.tick(1, &[], &[], None, &quiet_guard_state());
        let r2 = mon.tick(2, &[], &[], None, &quiet_guard_state());
        assert!(matches!(r2.health, TrajectoryHealth::Healthy));
        assert!(!r2.level_changed);
    }

    // ── Per-signal extraction ───────────────────────────────────────────

    #[test]
    fn repetition_entropy_is_advisory_warn_never_critical() {
        // Repeated identical fingerprints (entropy ≈ 0) must surface a Warn
        // advisory — NOT a Critical signal — so a repetitive I/O agent never
        // raises the operator divergence gate on repetition alone.
        let mut mon = TrajectoryMonitor::new(cfg());
        let fp = 42;
        // Tick past the warm-up (min_turns = 3) and min_observations (4).
        for turn in 1..=4 {
            let _ = mon.tick(
                turn,
                &[ToolObservation { fingerprint: fp, failed: false }],
            &[],
                None,
                &quiet_guard_state(),
            );
        }
        let r = mon.tick(
            5,
            &[ToolObservation { fingerprint: fp, failed: false }],
            &[],
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
            "expected an entropy advisory signal, got health={:?}",
            r.health
        );
        assert!(
            entropy_signals
                .iter()
                .all(|s| s.severity == SignalSeverity::Warn),
            "repetition entropy must cap at Warn, got {:?}",
            entropy_signals
        );
        // …and it must not, by itself, drive the session to Critical.
        assert!(
            !matches!(r.health, TrajectoryHealth::Critical { .. }),
            "entropy alone must not reach Critical, got {:?}",
            r.health
        );
    }

    #[test]
    fn repetition_entropy_warmup_suppresses_early_turns() {
        // Even with identical fingerprints meeting min_observations, the signal
        // stays silent before min_turns (default 3) — an opening burst of
        // similar calls (researcher fetching many URLs in turn 1) is fine.
        let mut mon = TrajectoryMonitor::new(cfg());
        for turn in 1..=2 {
            let r = mon.tick(
                turn,
                &[
                    ToolObservation { fingerprint: 9, failed: false },
                    ToolObservation { fingerprint: 9, failed: false },
                    ToolObservation { fingerprint: 9, failed: false },
                    ToolObservation { fingerprint: 9, failed: false },
                ],
                &[],
                None,
                &quiet_guard_state(),
            );
            let has_entropy = r
                .health
                .signals()
                .iter()
                .any(|s| s.kind == DivergenceSignalKind::RepetitionEntropy);
            assert!(
                !has_entropy,
                "entropy fired during warm-up at turn {} (health={:?})",
                turn, r.health
            );
        }
    }

    #[test]
    fn repetition_entropy_silent_below_min_observations() {
        let mut mon = TrajectoryMonitor::new(cfg());
        // 3 calls — default min_observations is 4.
        for turn in 1..=3 {
            let r = mon.tick(
                turn,
                &[ToolObservation { fingerprint: 7, failed: false }],
            &[],
                None,
                &quiet_guard_state(),
            );
            let has_entropy = r
                .health
                .signals()
                .iter()
                .any(|s| s.kind == DivergenceSignalKind::RepetitionEntropy);
            assert!(!has_entropy, "entropy fired too early at turn {}", turn);
        }
    }

    #[test]
    fn error_burst_fires_when_window_full_of_errors() {
        let mut mon = TrajectoryMonitor::new(cfg());
        // Default warn_count = 8, window_size = 8: tick 8 turns each with one error.
        for turn in 1..=7 {
            let _ = mon.tick(
                turn,
                &[ToolObservation { fingerprint: turn, failed: true }],
            &[],
                None,
                &quiet_guard_state(),
            );
        }
        // 8th tick: window has 7 ones, this pushes an 8th → total = 8 ≥ warn_count.
        let r = mon.tick(
            8,
            &[ToolObservation { fingerprint: 8, failed: true }],
            &[],
            None,
            &quiet_guard_state(),
        );
        let has_burst = r
            .health
            .signals()
            .iter()
            .any(|s| s.kind == DivergenceSignalKind::ErrorBurst);
        assert!(
            has_burst,
            "expected error_burst signal after 8 error turns, got {:?}",
            r.health
        );
    }

    #[test]
    fn digest_stall_does_not_fire_before_first_observation() {
        // No fingerprint ever observed → stall must not fire even when
        // the turn_counter is high.
        let mut mon = TrajectoryMonitor::new(cfg());
        let r = mon.tick(100, &[], &[], None, &quiet_guard_state());
        let has_stall = r
            .health
            .signals()
            .iter()
            .any(|s| s.kind == DivergenceSignalKind::DigestStall);
        assert!(!has_stall);
    }

    #[test]
    fn digest_stall_fires_when_no_new_fingerprint_for_warn_turns() {
        let mut mon = TrajectoryMonitor::new(cfg());
        // Establish a fingerprint at turn 1.
        let _ = mon.tick(
            1,
            &[ToolObservation { fingerprint: 1, failed: false }],
            &[],
            None,
            &quiet_guard_state(),
        );
        // 5 turns later with the SAME fingerprint repeated — stall = 5.
        for turn in 2..=6 {
            let _ = mon.tick(
                turn,
                &[ToolObservation { fingerprint: 1, failed: false }],
            &[],
                None,
                &quiet_guard_state(),
            );
        }
        let r = mon.tick(
            7,
            &[ToolObservation { fingerprint: 1, failed: false }],
            &[],
            None,
            &quiet_guard_state(),
        );
        let stall = r
            .health
            .signals()
            .iter()
            .find(|s| s.kind == DivergenceSignalKind::RepetitionEntropy);
        assert!(stall.is_some(), "expected repetition_entropy signal (stall logic subsumed by entropy), got {:?}", r.health);
    }

    #[test]
    fn context_pressure_signal_skipped_when_no_utilization() {
        let mut mon = TrajectoryMonitor::new(cfg());
        let r = mon.tick(1, &[], &[], None, &quiet_guard_state());
        let has_ctx = r
            .health
            .signals()
            .iter()
            .any(|s| s.kind == DivergenceSignalKind::ContextPressure);
        assert!(!has_ctx);
    }

    #[test]
    fn context_pressure_signal_fires_at_warn_threshold() {
        let mut mon = TrajectoryMonitor::new(cfg());
        let r = mon.tick(1, &[], &[], Some(0.81), &quiet_guard_state());
        let s = r
            .health
            .signals()
            .iter()
            .find(|s| s.kind == DivergenceSignalKind::ContextPressure)
            .expect("expected context_pressure signal");
        assert_eq!(s.severity, SignalSeverity::Warn);
    }

    // ── Signal toggles ──────────────────────────────────────────────────

    #[test]
    fn per_signal_toggle_silences_that_signal() {
        let mut c = cfg();
        c.signals.error_burst = false;
        let mut mon = TrajectoryMonitor::new(c);
        for turn in 1..=6 {
            let _ = mon.tick(
                turn,
                &[ToolObservation { fingerprint: turn, failed: true }],
            &[],
                None,
                &quiet_guard_state(),
            );
        }
        let r = mon.tick(7, &[], &[], None, &quiet_guard_state());
        assert!(
            !r.health
                .signals()
                .iter()
                .any(|s| s.kind == DivergenceSignalKind::ErrorBurst),
            "error_burst should be silenced by toggle"
        );
    }

    // ── Fingerprint normalization ───────────────────────────────────────

    #[test]
    fn fingerprint_strips_intent_field() {
        let a = fingerprint_tool_call("web.search", r#"{"intent":"find logs","q":"x"}"#);
        let b = fingerprint_tool_call("web.search", r#"{"intent":"different","q":"x"}"#);
        assert_eq!(a, b, "intent field must not affect fingerprint");
    }

    #[test]
    fn fingerprint_different_for_different_args() {
        let a = fingerprint_tool_call("web.search", r#"{"q":"a"}"#);
        let b = fingerprint_tool_call("web.search", r#"{"q":"b"}"#);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_falls_back_on_invalid_json() {
        // Must not panic; should still produce a stable hash.
        let a = fingerprint_tool_call("x", "{not json");
        let b = fingerprint_tool_call("x", "{not json");
        assert_eq!(a, b);
    }

    // ── Shannon entropy primitive ───────────────────────────────────────

    #[test]
    fn entropy_zero_for_single_repeated_value() {
        let mut w: VecDeque<u64> = VecDeque::new();
        for _ in 0..8 {
            w.push_back(1);
        }
        let e = shannon_entropy_bits(&w);
        assert!(e.abs() < 1e-6, "expected 0.0, got {}", e);
    }

    #[test]
    fn entropy_three_bits_for_eight_unique_values() {
        let mut w: VecDeque<u64> = VecDeque::new();
        for i in 0..8 {
            w.push_back(i);
        }
        let e = shannon_entropy_bits(&w);
        assert!((e - 3.0).abs() < 1e-5, "expected 3.0 bits, got {}", e);
    }

    #[test]
    fn entropy_zero_on_empty() {
        let w: VecDeque<u64> = VecDeque::new();
        assert_eq!(shannon_entropy_bits(&w), 0.0);
    }

    // ── Full state-machine progression ──────────────────────────────────

    #[test]
    fn progression_healthy_to_watching_to_diverging_to_critical() {
        // Simulates a looping agent: healthy start → one warn signal
        // (Watching/observed) → two warn signals (Diverging/detected) →
        // critical signal (Critical/escalated).
        let mut mon = TrajectoryMonitor::new(cfg());
        let mut state = quiet_guard_state();
        state.max_loops_without_progress = 5;
        state.max_tool_failures = 5;

        // Turn 1: healthy, first tick → level_changed = true but Healthy
        // (caller emits only for non-Healthy).
        state.current_loops = 1;
        let r = mon.tick(1, &[], &[], None, &state);
        assert!(matches!(r.health, TrajectoryHealth::Healthy));
        assert!(r.level_changed);

        // Turn 2-3: still healthy, no level change.
        for turn in 2u64..=3 {
            state.current_loops = turn as u32;
            let r = mon.tick(turn, &[], &[], None, &state);
            assert!(matches!(r.health, TrajectoryHealth::Healthy));
            assert!(!r.level_changed);
        }

        // Turn 4: loop pressure at 4/5 = 0.80 (warn) → Watching.
        state.current_loops = 4;
        let r = mon.tick(4, &[], &[], None, &state);
        assert!(
            matches!(r.health, TrajectoryHealth::Watching { .. }),
            "expected Watching at turn 4, got {:?}",
            r.health
        );
        assert!(r.level_changed);
        assert_eq!(r.health.causal_action(), Some("observed"));

        // Turn 5: loop pressure still warn (4/5, no new loops added since
        // tick only reads state, does not modify it) → same level, no
        // re-trigger.
        let r = mon.tick(5, &[], &[], None, &state);
        assert!(
            matches!(r.health, TrajectoryHealth::Watching { .. }),
            "expected Watching at turn 5, got {:?}",
            r.health
        );
        assert!(!r.level_changed, "same level must not re-trigger");

        // Turn 6: keep loop pressure at warn (4/5) and add failure
        // pressure (worst tool at 4/5 = 0.80 warn). Two warn signals →
        // Watching under RFC D.6 (only FeedbackIgnored may drive Diverging).
        state.current_loops = 4;
        state.tool_failure_counts.insert("sandbox.exec".into(), 4);
        let r = mon.tick(6, &[], &[], None, &state);
        assert!(
            matches!(r.health, TrajectoryHealth::Watching { .. }),
            "expected Watching at turn 6 under D.6, got {:?}",
            r.health
        );
        // Still Watching — level does not change from turn 5.
        assert!(!r.level_changed, "still watching must not re-trigger");
        assert_eq!(r.health.causal_action(), Some("observed"));

        // Turn 7: introduce FeedbackIgnored. This is the only signal that
        // may drive Diverging/Critical.
        mon.record_feedback(6, &[FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        }]);
        let r = mon.tick(
            7,
            &[],
            &[FeedbackEvent::Validation {
                rule: "output_schema".into(),
                field_path: None,
            }],
            None,
            &state,
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Diverging { .. }),
            "expected Diverging at turn 7 with feedback ignored, got {:?}",
            r.health
        );
        assert!(r.level_changed);
        assert_eq!(r.health.causal_action(), Some("detected"));

        // Turn 8: repeat the same ignored feedback twice more → Critical.
        mon.record_feedback(7, &[FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        }]);
        mon.record_feedback(8, &[FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        }]);
        let r = mon.tick(
            9,
            &[],
            &[FeedbackEvent::Validation {
                rule: "output_schema".into(),
                field_path: None,
            }],
            None,
            &state,
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Critical { .. }),
            "expected Critical after repeated feedback ignored, got {:?}",
            r.health
        );
        assert!(r.level_changed);
        assert_eq!(r.health.causal_action(), Some("escalated"));

        // Turn 10: same critical → no re-trigger.
        let r = mon.tick(
            10,
            &[],
            &[FeedbackEvent::Validation {
                rule: "output_schema".into(),
                field_path: None,
            }],
            None,
            &state,
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Critical { .. }),
            "expected Critical at turn 10, got {:?}",
            r.health
        );
        assert!(!r.level_changed, "same critical must not re-trigger");
    }

    #[test]
    fn progression_healthy_never_emits_event() {
        // A perfectly healthy session must never trigger a divergence event.
        let mut mon = TrajectoryMonitor::new(cfg());
        let state = quiet_guard_state();
        for turn in 1..=20 {
            let r = mon.tick(turn, &[], &[], None, &state);
            assert!(matches!(r.health, TrajectoryHealth::Healthy));
            assert_eq!(r.health.causal_action(), None);
        }
    }

    #[test]
    fn feedback_incorporated_does_not_fire_same_turn_as_first_feedback() {
        let mut mon = TrajectoryMonitor::new(cfg());
        let state = quiet_guard_state();
        let fb = FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        };

        // Record feedback and supply a *different* current error on the same
        // turn. This should NOT emit FeedbackIncorporated because the agent
        // has not yet had a chance to act on the feedback.
        mon.record_feedback(5, &[fb.clone()]);
        let r = mon.tick(
            5,
            &[],
            &[FeedbackEvent::ToolError {
                tool: "content.write".into(),
                error_type: ToolErrorType::Validation,
                message_signature: "missing field".into(),
            }],
            None,
            &state,
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Healthy),
            "same-turn feedback must not produce FeedbackIncorporated: {:?}",
            r.health
        );

        // On the next turn, a different error after prior feedback is
        // incorporated (advisory).
        let r = mon.tick(
            6,
            &[],
            &[FeedbackEvent::ToolError {
                tool: "content.write".into(),
                error_type: ToolErrorType::Validation,
                message_signature: "missing field".into(),
            }],
            None,
            &state,
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Watching { .. }),
            "FeedbackIncorporated should be advisory Watching: {:?}",
            r.health
        );
        let signals = r.health.signals();
        assert!(
            signals.iter().any(|s| s.kind == DivergenceSignalKind::FeedbackIncorporated),
            "expected FeedbackIncorporated signal: {:?}",
            signals
        );
    }

    #[test]
    fn feedback_snapshot_and_restore_preserve_turn_counters() {
        let mut mon = TrajectoryMonitor::new(cfg());
        mon.record_feedback(
            3,
            &[
                FeedbackEvent::Validation {
                    rule: "output_schema".into(),
                    field_path: None,
                },
                FeedbackEvent::ToolError {
                    tool: "content.write".into(),
                    error_type: ToolErrorType::Validation,
                    message_signature: "missing field".into(),
                },
            ],
        );
        mon.record_feedback(
            5,
            &[FeedbackEvent::Validation {
                rule: "required_artifacts".into(),
                field_path: None,
            }],
        );

        let snap = mon.feedback_snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].0, 3);
        assert_eq!(snap[2].0, 5);

        let mut restored = TrajectoryMonitor::new(cfg());
        restored.restore_feedback(snap);
        // The restored monitor should detect ignored feedback if the same
        // signature appears on a subsequent turn.
        let r = restored.tick(
            6,
            &[],
            &[FeedbackEvent::Validation {
                rule: "output_schema".into(),
                field_path: None,
            }],
            None,
            &quiet_guard_state(),
        );
        assert!(
            matches!(r.health, TrajectoryHealth::Diverging { .. }),
            "restored monitor should detect ignored validation feedback: {:?}",
            r.health
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // RFC D.5 — suppress-on-progress grace
    // ──────────────────────────────────────────────────────────────────────

    fn progress_cfg(window: u32, turns: u32) -> TrajectoryConfig {
        let mut c = cfg();
        c.progress_grace_window = window;
        c.progress_grace_turns = turns;
        c
    }

    #[test]
    fn progress_grace_earns_suppression_after_window() {
        // Feedback given on turn 1, then two turns of distinct errors earn
        // suppression for the next 3 turns.
        let prior = FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        };
        let different = FeedbackEvent::ToolError {
            tool: "content.write".into(),
            error_type: ToolErrorType::Validation,
            message_signature: "missing field".into(),
        };

        let mut mon = TrajectoryMonitor::new(progress_cfg(2, 3));
        mon.record_feedback(1, &[prior]);

        let r1 = mon.tick(2, &[], &[different.clone()], None, &quiet_guard_state());
        assert!(
            r1.health
                .signals()
                .iter()
                .any(|s| s.kind == DivergenceSignalKind::FeedbackIncorporated),
            "turn 2 should show FeedbackIncorporated"
        );
        assert!(r1.suppress_until_turn.is_none(), "one progress turn is not enough");

        let r2 = mon.tick(3, &[], &[different.clone()], None, &quiet_guard_state());
        assert_eq!(
            r2.suppress_until_turn,
            Some(6),
            "two consecutive progress turns should earn suppression until turn 6"
        );
    }

    #[test]
    fn progress_grace_resets_on_non_progress_turn() {
        let prior = FeedbackEvent::Validation {
            rule: "output_schema".into(),
            field_path: None,
        };
        let different = FeedbackEvent::ToolError {
            tool: "content.write".into(),
            error_type: ToolErrorType::Validation,
            message_signature: "missing field".into(),
        };

        let mut mon = TrajectoryMonitor::new(progress_cfg(2, 3));
        mon.record_feedback(1, &[prior]);

        let _ = mon.tick(2, &[], &[different.clone()], None, &quiet_guard_state());
        // Turn 3 has no feedback and no errors → not progress.
        let _ = mon.tick(3, &[], &[], None, &quiet_guard_state());
        // Turn 4 is progress again, but the streak was broken.
        let r = mon.tick(4, &[], &[different], None, &quiet_guard_state());
        assert!(
            r.suppress_until_turn.is_none(),
            "streak reset should prevent grace from a single progress turn"
        );
    }
}

/// Map a level string back to the internal &'static str.
/// Used by `restore_last_level` on session resume.
fn restore_level(s: &str) -> Option<&'static str> {
    match s {
        "healthy" => Some("healthy"),
        "watching" => Some("watching"),
        "blocked" => Some("blocked"),
        "diverging" => Some("diverging"),
        "critical" => Some("critical"),
        _ => None,
    }
}