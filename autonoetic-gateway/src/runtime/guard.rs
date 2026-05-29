//! Loop Guard Mechanism.
//!
//! Prevents agents from getting stuck in infinite reasoning loops
//! without making progress.
//!
//! Independent trip conditions:
//! 1. **Max loops without progress**: Agent executed N cycles without any
//!    *meaningful* tool call resetting the counter.
//! 2. **Tool failure budget exhausted**: A single tool has failed more than
//!    `max_tool_failures` times total in the session, regardless of arguments
//!    or targets. This catches alternating-failure patterns.
//! 3. **Rotating-polling pattern (sister rule to P-7.7, issue #287)**: The
//!    last `max_window_size` successful tool calls contain only
//!    `max_distinct_floor` or fewer distinct (tool, args) fingerprints. This
//!    catches agents that cycle through a small set of read-only tools
//!    (e.g. `workflow.wait → workflow.state → content.read → artifact.inspect
//!    → agent.exists → workflow.wait …`) without making semantic progress —
//!    a pattern that defeats trip condition #1 because each call has a
//!    different fingerprint and resets `current_loops`.
//! 4. **Child failure budget**: A separate budget for child task failures.
//!
//! "Meaningful progress" is determined by a fingerprint of the last
//! successful tool call. Repeated calls with the same (tool, arguments)
//! hash do not count as new progress — the agent is spinning on the same
//! operation. A call is considered "repeated" after
//! `max_consecutive_same_progress` consecutive occurrences.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use autonoetic_types::config::LoopGuardConfig;
use autonoetic_types::tool_error::ToolErrorType;

/// Why the loop guard tripped, set by `register_progress` / `check_loop` and
/// surfaced through [`LoopGuard::last_trip_reason`] so the caller can emit
/// a structured causal event before propagating the trip as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopGuardTripReason {
    /// Trip condition #1 — `current_loops >= max_loops_without_progress`.
    NoMeaningfulProgress { cycles: u32 },
    /// Trip condition #2 — a single tool exceeded `max_tool_failures`.
    ToolFailureBudget { tool: String, failures: u32 },
    /// Trip condition #3 — recent window of successful calls has too
    /// few distinct fingerprints. Catches rotating polling.
    RotatingPollingPattern {
        window_size: usize,
        distinct_count: usize,
        floor: usize,
    },
    /// Trip condition #4 — child task failures exceeded `max_child_failures`.
    ChildFailureBudget { failures: u32 },
}

impl LoopGuardTripReason {
    /// Stable identifier for causal event payloads.
    pub fn code(&self) -> &'static str {
        match self {
            LoopGuardTripReason::NoMeaningfulProgress { .. } => "no_meaningful_progress",
            LoopGuardTripReason::ToolFailureBudget { .. } => "tool_failure_budget",
            LoopGuardTripReason::RotatingPollingPattern { .. } => "rotating_polling_pattern",
            LoopGuardTripReason::ChildFailureBudget { .. } => "child_failure_budget",
        }
    }

    /// Constitutional rule this trip enforces. Used to populate
    /// `enforced_rules` on the `loop_guard.tripped` causal event so the
    /// audit chain attributes each trip to the rule whose text actually
    /// describes it (rather than blanket-labelling every trip P-7.7).
    ///
    /// - `ToolFailureBudget`     → P-7.5 (per-tool failure budget)
    /// - `NoMeaningfulProgress`  → P-7.7 (consecutive steps w/o successful result)
    /// - `RotatingPollingPattern`→ P-7.19 (no semantic progress across successes)
    /// - `ChildFailureBudget`    → P-7.20 (child-failure delegation-loop budget)
    pub fn rule_id(&self) -> &'static str {
        match self {
            LoopGuardTripReason::ToolFailureBudget { .. } => "P-7.5",
            LoopGuardTripReason::NoMeaningfulProgress { .. } => "P-7.7",
            LoopGuardTripReason::RotatingPollingPattern { .. } => "P-7.19",
            LoopGuardTripReason::ChildFailureBudget { .. } => "P-7.20",
        }
    }
}

pub struct LoopGuard {
    max_loops_without_progress: u32,
    max_tool_failures: u32,
    max_consecutive_same_progress: u32,
    max_child_failures: u32,
    /// Trip condition #3 — recent-call window cap.
    max_window_size: usize,
    /// Trip condition #3 — minimum distinct fingerprints required to clear.
    max_distinct_floor: usize,
    /// From gateway config — max loop resets attributable to each tool name.
    progress_budget_tools: HashMap<String, u32>,
    /// How many times each budgeted tool has reset `current_loops` this session.
    progress_budget_used: HashMap<String, u32>,
    current_loops: u32,
    tool_failure_counts: std::collections::HashMap<String, u32>,
    last_progress_fingerprint: Option<(String, u64)>,
    consecutive_progress_count: u32,
    child_failure_count: u32,
    /// Sliding window of fingerprint hashes for the last
    /// `max_window_size` successful tool calls. Used by trip condition #3
    /// (rotating-polling detector).
    recent_fingerprints: VecDeque<u64>,
    /// Trip reason recorded when any condition fires. Cleared on construction
    /// and never reset — once a guard has tripped, subsequent calls are
    /// errors. `last_trip_reason` exposes this for causal-event emission.
    trip_reason: Option<LoopGuardTripReason>,
}

impl LoopGuard {
    pub fn new(max_loops_without_progress: u32) -> Self {
        Self {
            max_loops_without_progress,
            max_tool_failures: 8,
            max_consecutive_same_progress: 1,
            max_child_failures: 5,
            max_window_size: default_rotation_window_size(),
            max_distinct_floor: default_rotation_distinct_floor(),
            progress_budget_tools: HashMap::new(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            recent_fingerprints: VecDeque::new(),
            trip_reason: None,
        }
    }

    pub fn with_config(cfg: &LoopGuardConfig) -> Self {
        Self {
            max_loops_without_progress: cfg.max_loops_without_progress,
            max_tool_failures: cfg.max_tool_failures,
            max_consecutive_same_progress: cfg.max_consecutive_same_progress,
            max_child_failures: cfg.max_child_failures,
            max_window_size: cfg.rotation_window_size,
            max_distinct_floor: cfg.rotation_distinct_floor,
            progress_budget_tools: cfg.progress_budget_tools.clone(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            recent_fingerprints: VecDeque::new(),
            trip_reason: None,
        }
    }

    pub fn check_loop(&mut self) -> anyhow::Result<()> {
        // 0. Pre-set trip reason (e.g. from `register_progress` detecting a
        //    rotating-polling pattern) wins over the live checks below — it
        //    captures *why* the guard was already over budget when the new
        //    iteration started.
        if let Some(reason) = self.trip_reason.clone() {
            return Err(format_trip_error(&reason));
        }

        if self.current_loops >= self.max_loops_without_progress {
            let reason = LoopGuardTripReason::NoMeaningfulProgress {
                cycles: self.current_loops,
            };
            self.trip_reason = Some(reason.clone());
            return Err(format_trip_error(&reason));
        }

        for (tool_name, count) in &self.tool_failure_counts {
            if *count >= self.max_tool_failures {
                let reason = LoopGuardTripReason::ToolFailureBudget {
                    tool: tool_name.clone(),
                    failures: *count,
                };
                self.trip_reason = Some(reason.clone());
                return Err(format_trip_error(&reason));
            }
        }

        if self.child_failure_count >= self.max_child_failures {
            let reason = LoopGuardTripReason::ChildFailureBudget {
                failures: self.child_failure_count,
            };
            self.trip_reason = Some(reason.clone());
            return Err(format_trip_error(&reason));
        }

        self.current_loops += 1;
        Ok(())
    }

    /// Returns the trip reason if the guard has already tripped, so the
    /// caller can emit a structured causal event before propagating the
    /// error. Returns `None` when the guard is still healthy.
    pub fn last_trip_reason(&self) -> Option<&LoopGuardTripReason> {
        self.trip_reason.as_ref()
    }

    /// Returns `true` when the guard is approaching a trip condition but has
    /// not yet tripped. Specifically, when `current_loops >= 80%` of
    /// `max_loops_without_progress`, or any single tool failure count has
    /// reached 80% of `max_tool_failures`.
    ///
    /// This is the trigger for P-7.18 degraded-mode entry via loop-guard
    /// sub-trip warnings.
    pub fn is_sub_trip_warning(&self) -> bool {
        let loop_threshold = ((self.max_loops_without_progress as u64 * 4 + 4) / 5) as u32;
        if self.current_loops >= loop_threshold && self.current_loops < self.max_loops_without_progress {
            return true;
        }
        let failure_threshold = ((self.max_tool_failures as u64 * 4 + 4) / 5) as u32;
        for count in self.tool_failure_counts.values() {
            if *count >= failure_threshold && *count < self.max_tool_failures {
                return true;
            }
        }
        false
    }

    /// Track a tool failure — failures accumulate per tool name regardless of arguments.
    ///
    /// Permission errors are excluded entirely: the agent cannot fix by
    /// retrying — it needs authorization.
    pub fn register_failure(
        &mut self,
        tool_name: &str,
        _arguments: &str,
        error_type: Option<&ToolErrorType>,
    ) {
        if matches!(error_type, Some(ToolErrorType::Permission)) {
            return;
        }
        *self
            .tool_failure_counts
            .entry(tool_name.to_string())
            .or_insert(0) += 1;
    }

    /// Track a child agent task failure (from workflow.wait returning any_failed: true).
    /// Counts against a separate budget from tool failures — a planner can only waste
    /// so many delegation rounds before tripping. Unlike tool failures, this does NOT
    /// reset on progress — once a child fails, that's a permanent budget hit.
    pub fn register_child_failure(&mut self) {
        self.child_failure_count += 1;
    }

    /// Track a successful tool call. Only counts as "progress" (resets current_loops)
    /// if this is a different tool call than the last successful one, or if the same
    /// tool+args has not repeated more than `max_consecutive_same_progress` times.
    /// This prevents agents from spinning on repeated identical successful calls
    /// (e.g., web.search returning the same cached results).
    pub fn register_progress(&mut self, tool_name: &str, arguments: &str) {
        self.register_progress_inner(tool_name, arguments, false);
    }

    /// Track a successful tool call whose result carried
    /// `side_effect_state: "committed"` (P-5.14 / P-6.26). This is treated
    /// as terminal-progress evidence — the rotating-polling detector window
    /// is cleared because a real side effect just landed. The classical
    /// progress accounting (max_loops_without_progress / consecutive
    /// progress count) is otherwise unchanged.
    pub fn register_progress_terminal(&mut self, tool_name: &str, arguments: &str) {
        self.register_progress_inner(tool_name, arguments, true);
    }

    fn register_progress_inner(
        &mut self,
        tool_name: &str,
        arguments: &str,
        terminal_side_effect: bool,
    ) {
        let fp = compute_fingerprint(
            tool_name,
            normalize_arguments_for_progress_fingerprint(arguments).as_ref(),
        );

        // Trip condition #3: rotating-polling detector.
        //
        // - A terminal side effect (per P-5.14) clears the window — the
        //   agent just made committed progress, so any prior monotony is
        //   stale.
        // - Otherwise, append the new fingerprint, evicting the oldest if
        //   the window is full. Then check whether the windowed distinct
        //   count has dropped to or below the configured floor.
        if terminal_side_effect {
            self.recent_fingerprints.clear();
        } else if self.max_window_size > 0 {
            if self.recent_fingerprints.len() >= self.max_window_size {
                self.recent_fingerprints.pop_front();
            }
            self.recent_fingerprints.push_back(fp.1);

            if self.recent_fingerprints.len() >= self.max_window_size {
                let distinct: HashSet<&u64> = self.recent_fingerprints.iter().collect();
                if distinct.len() <= self.max_distinct_floor {
                    self.trip_reason = Some(LoopGuardTripReason::RotatingPollingPattern {
                        window_size: self.recent_fingerprints.len(),
                        distinct_count: distinct.len(),
                        floor: self.max_distinct_floor,
                    });
                }
            }
        }

        let is_new = self.last_progress_fingerprint.as_ref() != Some(&fp);

        if is_new {
            self.consecutive_progress_count = 1;
        } else {
            self.consecutive_progress_count += 1;
        }

        let would_reset_loops = is_new
            || self.consecutive_progress_count <= self.max_consecutive_same_progress;

        if would_reset_loops {
            let allowed_by_budget = match self.progress_budget_tools.get(tool_name) {
                None => true,
                Some(&budget) => {
                    let used = self
                        .progress_budget_used
                        .entry(tool_name.to_string())
                        .or_insert(0);
                    if *used >= budget {
                        false
                    } else {
                        *used += 1;
                        true
                    }
                }
            };
            if allowed_by_budget {
                self.current_loops = 0;
            }
        }

        self.last_progress_fingerprint = Some(fp);
    }

    pub fn snapshot(&self) -> LoopGuardState {
        LoopGuardState {
            max_loops_without_progress: self.max_loops_without_progress,
            max_tool_failures: self.max_tool_failures,
            max_consecutive_same_progress: self.max_consecutive_same_progress,
            max_child_failures: self.max_child_failures,
            max_window_size: self.max_window_size,
            max_distinct_floor: self.max_distinct_floor,
            progress_budget_tools: self.progress_budget_tools.clone(),
            progress_budget_used: self.progress_budget_used.clone(),
            current_loops: self.current_loops,
            tool_failure_counts: self.tool_failure_counts.clone(),
            last_progress_fingerprint: self.last_progress_fingerprint.clone(),
            consecutive_progress_count: self.consecutive_progress_count,
            child_failure_count: self.child_failure_count,
            recent_fingerprints: self.recent_fingerprints.iter().copied().collect(),
        }
    }

    pub fn restore(state: LoopGuardState) -> Self {
        Self {
            max_loops_without_progress: state.max_loops_without_progress,
            max_tool_failures: state.max_tool_failures,
            max_consecutive_same_progress: state.max_consecutive_same_progress,
            max_child_failures: state.max_child_failures,
            max_window_size: state.max_window_size,
            max_distinct_floor: state.max_distinct_floor,
            progress_budget_tools: state.progress_budget_tools,
            progress_budget_used: state.progress_budget_used,
            current_loops: state.current_loops,
            tool_failure_counts: state.tool_failure_counts,
            last_progress_fingerprint: state.last_progress_fingerprint,
            consecutive_progress_count: state.consecutive_progress_count,
            child_failure_count: state.child_failure_count,
            recent_fingerprints: state.recent_fingerprints.into_iter().collect(),
            trip_reason: None,
        }
    }
}

fn format_trip_error(reason: &LoopGuardTripReason) -> anyhow::Error {
    match reason {
        LoopGuardTripReason::NoMeaningfulProgress { cycles } => anyhow::anyhow!(
            "LoopGuard tripped: Agent executed {} cycles without meaningful progress.",
            cycles
        ),
        LoopGuardTripReason::ToolFailureBudget { tool, failures } => anyhow::anyhow!(
            "LoopGuard tripped: Tool '{}' has failed {} times in this session. \
             Breaking loop to prevent resource waste.",
            tool,
            failures
        ),
        LoopGuardTripReason::RotatingPollingPattern {
            window_size,
            distinct_count,
            floor,
        } => anyhow::anyhow!(
            "LoopGuard tripped: rotating-polling pattern detected — the last {} \
             successful tool calls only used {} distinct (tool, args) fingerprint(s), \
             at or below the configured floor of {}. The agent is cycling through a \
             small set of read-only tools without making semantic progress. \
             Switch strategy or escalate (issue #287).",
            window_size,
            distinct_count,
            floor
        ),
        LoopGuardTripReason::ChildFailureBudget { failures } => anyhow::anyhow!(
            "LoopGuard tripped: {} child agent tasks have failed in this session. \
             Breaking delegation loop — escalate to human or change strategy.",
            failures
        ),
    }
}

fn default_rotation_window_size() -> usize {
    16
}

fn default_rotation_distinct_floor() -> usize {
    6
}

/// Strips echoed / non-schema fields from tool `arguments_json` before progress
/// fingerprints. Models often attach a changing `"intent"` string; hashing the raw
/// JSON would otherwise treat repeated semantically-identical calls as new
/// progress and disable `max_loops_without_progress` protection.
fn normalize_arguments_for_progress_fingerprint(arguments: &str) -> std::borrow::Cow<'_, str> {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return std::borrow::Cow::Borrowed(arguments);
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("intent");
    }
    match serde_json::to_string(&v) {
        Ok(s) => std::borrow::Cow::Owned(s),
        Err(_) => std::borrow::Cow::Borrowed(arguments),
    }
}

fn compute_fingerprint(tool_name: &str, arguments: &str) -> (String, u64) {
    let mut hasher = DefaultHasher::new();
    tool_name.hash(&mut hasher);
    arguments.hash(&mut hasher);
    (tool_name.to_string(), hasher.finish())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoopGuardState {
    pub max_loops_without_progress: u32,
    pub max_tool_failures: u32,
    pub max_consecutive_same_progress: u32,
    pub max_child_failures: u32,
    /// Rotating-polling detector window (issue #287). Legacy checkpoints
    /// without this field default to the current build's window size, so
    /// they keep working without redumping.
    #[serde(default = "default_rotation_window_size")]
    pub max_window_size: usize,
    /// Rotating-polling detector distinct-count floor (issue #287).
    #[serde(default = "default_rotation_distinct_floor")]
    pub max_distinct_floor: usize,
    #[serde(default)]
    pub progress_budget_tools: HashMap<String, u32>,
    #[serde(default)]
    pub progress_budget_used: HashMap<String, u32>,
    pub current_loops: u32,
    pub tool_failure_counts: std::collections::HashMap<String, u32>,
    pub last_progress_fingerprint: Option<(String, u64)>,
    pub consecutive_progress_count: u32,
    pub child_failure_count: u32,
    /// Sliding window of recent successful-call fingerprints. Legacy
    /// checkpoints come back with an empty window; this is safe because
    /// the rotating-polling detector only trips once the window fills.
    #[serde(default)]
    pub recent_fingerprints: Vec<u64>,
}

impl Default for LoopGuardState {
    fn default() -> Self {
        Self {
            max_loops_without_progress: 10,
            max_tool_failures: 8,
            max_consecutive_same_progress: 1,
            max_child_failures: 5,
            max_window_size: default_rotation_window_size(),
            max_distinct_floor: default_rotation_distinct_floor(),
            progress_budget_tools: HashMap::new(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            recent_fingerprints: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_guard_trips_on_max_loops() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_progress_resets_loops() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        guard.register_progress("web_fetch", r#"{"url":"https://a.com"}"#);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_err());
    }

    /// Regression: LLMs sometimes add a superficially different `"intent"` string on every
    /// tool call while the semantic args are unchanged. Without normalization, fingerprints
    /// never match → every call looks like fresh progress → the loop counter never climbs.
    #[test]
    fn repeating_same_tool_with_only_intent_varying_trips_guard() {
        let cfg = autonoetic_types::config::LoopGuardConfig {
            max_loops_without_progress: 5,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);
        for epoch in 0usize..7usize {
            if epoch == 6 {
                assert!(guard.check_loop().is_err());
                return;
            }
            assert!(guard.check_loop().is_ok(), "epoch {}", epoch);
            guard.register_progress(
                "agent_exists",
                &format!(
                    r#"{{"agent_id":"weather.default","intent":"check-{epoch}"}}"#
                ),
            );
        }
        unreachable!("check_loop did not trip");
    }

    #[test]
    fn test_loop_guard_trips_on_tool_failure_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..7 {
            guard.register_failure("web_fetch", r#"{"url":"https://example.com/a"}"#, None);
            guard.register_progress("web_fetch", r#"{"url":"https://example.com/a"}"#);
            assert!(guard.check_loop().is_ok());
        }
        guard.register_failure("web_fetch", r#"{"url":"https://example.com/z"}"#, None);
        assert!(guard.check_loop().is_err());
    }

    /// Child-failure budget trip (P-7.20). Child failures accumulate and do
    /// NOT reset on progress; the guard trips once `max_child_failures`
    /// (default 3) is reached. Pinning test cited by the enforcement
    /// register for P-7 / P-7.20.
    #[test]
    fn test_loop_guard_trips_on_child_failures() {
        let mut guard = LoopGuard::new(100); // high loop budget so the child budget is the trip cause
        guard.register_child_failure();
        guard.register_child_failure();
        assert!(guard.check_loop().is_ok(), "2 < default max_child_failures(3)");
        guard.register_child_failure(); // now 3
        let err = guard.check_loop().expect_err("3 >= max_child_failures must trip");
        assert!(err.to_string().contains("child"), "unexpected error: {err}");
        assert!(matches!(
            guard.last_trip_reason(),
            Some(crate::runtime::guard::LoopGuardTripReason::ChildFailureBudget { .. })
        ));
    }

    #[test]
    fn test_loop_guard_alternating_hosts_exhausts_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for i in 0..7 {
            let url = if i % 2 == 0 {
                "https://accuweather.com/a"
            } else {
                "https://weather.com/b"
            };
            guard.register_failure("web_fetch", &format!(r#"{{"url":"{}"}}"#, url), None);
            guard.register_progress("web_fetch", &format!(r#"{{"url":"{}"}}"#, url));
            assert!(guard.check_loop().is_ok());
        }

        guard.register_failure("web_fetch", r#"{"url":"https://accuweather.com/e"}"#, None);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_different_tools_tracked_separately() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..7 {
            guard.register_failure("web_fetch", r#"{"url":"https://example.com"}"#, None);
            guard.register_failure("sandbox_exec", r#"{"command":"python3 test.py"}"#, None);
            guard.register_progress("sandbox_exec", r#"{"command":"python3 test.py"}"#);
            assert!(guard.check_loop().is_ok());
        }

        guard.register_failure("web_fetch", r#"{"url":"https://example.com"}"#, None);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_non_url_tools_count_failures() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..7 {
            guard.register_failure("sandbox_exec", r#"{"command":"python3 test.py"}"#, None);
            guard.register_progress("sandbox_exec", r#"{"command":"python3 test.py"}"#);
            assert!(guard.check_loop().is_ok());
        }
        guard.register_failure("sandbox_exec", r#"{"command":"python3 other.py"}"#, None);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_snapshot_restore() {
        let mut guard = LoopGuard::new(3);
        guard.register_failure("web_fetch", r#"{"url":"https://example.com"}"#, None);
        guard.register_failure("web_fetch", r#"{"url":"https://other.com"}"#, None);
        assert_eq!(guard.check_loop().unwrap(), ());

        let snap = guard.snapshot();
        let restored = LoopGuard::restore(snap);

        assert_eq!(restored.current_loops, 1);
        assert_eq!(*restored.tool_failure_counts.get("web_fetch").unwrap(), 2);
    }

    #[test]
    fn test_repeated_same_tool_call_does_not_reset() {
        let mut guard = LoopGuard::new(4);

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_permission_errors_do_not_count_against_budget() {
        let mut guard = LoopGuard::new(100);

        for _ in 0..10 {
            guard.register_failure(
                "web_fetch",
                r#"{"url":"https://denied.com"}"#,
                Some(&ToolErrorType::Permission),
            );
            guard.register_progress("web_fetch", r#"{"url":"https://denied.com"}"#);
            assert!(guard.check_loop().is_ok());
        }

        guard.register_failure(
            "web_fetch",
            r#"{"url":"https://denied.com"}"#,
            Some(&ToolErrorType::Permission),
        );
        assert!(guard.check_loop().is_ok());
    }

    #[test]
    fn test_validation_errors_count_against_tool_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..7 {
            guard.register_failure(
                "web_fetch",
                r#"{"url":"https://bad.com"}"#,
                Some(&ToolErrorType::Validation),
            );
            guard.register_progress("web_fetch", r#"{"url":"https://bad.com"}"#);
            assert!(guard.check_loop().is_ok());
        }
        guard.register_failure(
            "web_fetch",
            r#"{"url":"https://bad.com"}"#,
            Some(&ToolErrorType::Validation),
        );
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_different_tool_resets_consecutive_count() {
        let mut guard = LoopGuard::new(4);

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("content_read", r#"{"name":"file.txt"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_different_args_resets_consecutive_count() {
        let mut guard = LoopGuard::new(4);

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"Paris weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"London weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"London weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"London weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"London weather"}"#);
        assert!(guard.check_loop().is_ok());

        guard.register_progress("web_search", r#"{"query":"London weather"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_alternating_same_calls_eventually_trip() {
        let mut guard = LoopGuard::new(3);

        for i in 0..6 {
            let q = if i % 2 == 0 {
                r#"{"query":"weather"}"#
            } else {
                r#"{"query":"forecast"}"#
            };
            guard.register_progress("web_search", q);
            guard.check_loop().ok();
        }

        // The alternating pattern keeps resetting current_loops because each
        // fingerprint is different. Now call check_loop without progress to exhaust the budget.
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_err());
    }

    /// After N successful `knowledge_store` calls, further ones must not reset
    /// `current_loops` (unique args would otherwise defeat `max_loops_without_progress`).
    #[test]
    fn knowledge_store_progress_budget_exhausts_then_trips() {
        let mut cfg = autonoetic_types::config::LoopGuardConfig::default();
        cfg.max_loops_without_progress = 5;
        cfg.progress_budget_tools = [("knowledge_store".to_string(), 3u32)]
            .into_iter()
            .collect();

        let mut guard = LoopGuard::with_config(&cfg);
        for i in 0..3 {
            assert!(guard.check_loop().is_ok(), "epoch {i}");
            guard.register_progress(
                "knowledge_store",
                &format!(r#"{{"id":"note-{i}","content":"x"}}"#),
            );
        }
        for _ in 0..5 {
            assert!(guard.check_loop().is_ok());
            guard.register_progress("knowledge_store", r#"{"id":"note-overflow","content":"y"}"#);
        }
        assert!(guard.check_loop().is_err());
    }

    // ──────────────────────────────────────────────────────────────────────
    // Rotating-polling detector tests (issue #287)
    // ──────────────────────────────────────────────────────────────────────

    /// Helper: a guard configured with a small window so the detector trips
    /// quickly in test fixtures. Defaults `max_loops_without_progress` very
    /// high so the rotating-polling path is the only one that can trip.
    fn rotation_guard(window: usize, floor: usize) -> LoopGuard {
        let mut cfg = autonoetic_types::config::LoopGuardConfig::default();
        cfg.max_loops_without_progress = 10_000;
        cfg.rotation_window_size = window;
        cfg.rotation_distinct_floor = floor;
        LoopGuard::with_config(&cfg)
    }

    /// The 880-turn antipattern: rotate through 5 distinct read-only tools
    /// indefinitely. The classical loop guard never trips because every call
    /// has a fresh fingerprint and resets `current_loops`. The detector
    /// must catch this once the window fills.
    #[test]
    fn rotating_polling_pattern_with_five_tools_trips() {
        let mut guard = rotation_guard(8, 5);
        let rotation = [
            ("workflow.wait", r#"{"task_ids":["t1"]}"#),
            ("workflow.state", r#"{}"#),
            ("content.read", r#"{"name":"f"}"#),
            ("artifact.inspect", r#"{"artifact_ref":"a"}"#),
            ("agent.exists", r#"{"agent_id":"x"}"#),
        ];
        // Loop the rotation enough times to fill the window twice over.
        // First trip should fire by the time we've registered 8 calls.
        for cycle in 0..3 {
            for (tool, args) in &rotation {
                guard.register_progress(tool, args);
                if guard.last_trip_reason().is_some() {
                    let reason = guard.last_trip_reason().unwrap().clone();
                    assert!(
                        matches!(
                            reason,
                            LoopGuardTripReason::RotatingPollingPattern { .. }
                        ),
                        "wrong trip reason: {:?}",
                        reason
                    );
                    // check_loop must surface the trip.
                    let err = guard.check_loop().unwrap_err();
                    assert!(
                        err.to_string().contains("rotating-polling pattern"),
                        "unexpected error: {err}"
                    );
                    return;
                }
            }
            let _ = cycle;
        }
        panic!("rotating-polling detector failed to trip after 3 full cycles");
    }

    /// Healthy varied work — 10 distinct tool calls in a window of 8 — must
    /// not trip the detector. (The floor is 5; distinct count stays at 8.)
    #[test]
    fn varied_healthy_work_does_not_trip_rotation_detector() {
        let mut guard = rotation_guard(8, 5);
        let calls = [
            ("agent.spawn", r#"{"agent_id":"a","input":"foo"}"#),
            ("content.write", r#"{"name":"out","content":"x"}"#),
            ("knowledge.write", r#"{"id":"n1","content":"y"}"#),
            ("artifact.build", r#"{"agent_id":"a"}"#),
            ("agent.revision.create_from_intent", r#"{"agent_id":"a"}"#),
            ("workflow.state", r#"{}"#),
            ("agent.revision.promote", r#"{"agent_id":"a","revision_id":"r1"}"#),
            ("agent.spawn", r#"{"agent_id":"b","input":"bar"}"#),
        ];
        for (tool, args) in &calls {
            guard.register_progress(tool, args);
        }
        assert!(guard.last_trip_reason().is_none(), "varied work tripped");
        assert!(guard.check_loop().is_ok());
    }

    /// With the default config (window 16, floor 6) the rotation of 5
    /// tools repeated through 16 calls must trip.
    #[test]
    fn rotating_polling_pattern_trips_with_default_config() {
        let mut guard = LoopGuard::with_config(&autonoetic_types::config::LoopGuardConfig::default());
        let rotation = [
            ("workflow.wait", r#"{}"#),
            ("workflow.state", r#"{}"#),
            ("content.read", r#"{}"#),
            ("artifact.inspect", r#"{}"#),
            ("agent.exists", r#"{}"#),
        ];
        for _ in 0..5 {
            for (tool, args) in &rotation {
                guard.register_progress(tool, args);
            }
        }
        assert!(
            guard.last_trip_reason().is_some(),
            "default config did not trip on 5-tool rotation over 25 calls"
        );
    }

    /// Terminal-progress events (P-5.14 `side_effect_state: committed`)
    /// clear the rotation window. An agent rotating on read-only tools
    /// can survive past the trip threshold if it interleaves a terminal
    /// event before the window fills.
    #[test]
    fn terminal_progress_clears_rotation_window() {
        let mut guard = rotation_guard(8, 5);
        let rotation = [
            ("workflow.wait", r#"{"task_ids":["t1"]}"#),
            ("workflow.state", r#"{}"#),
            ("content.read", r#"{"name":"f"}"#),
            ("artifact.inspect", r#"{"artifact_ref":"a"}"#),
        ];
        // Register 4 read-only calls (below the window threshold).
        for (tool, args) in &rotation {
            guard.register_progress(tool, args);
        }
        // A committed side effect clears the window.
        guard.register_progress_terminal("agent.revision.promote", r#"{}"#);
        // Now register the same 4 read-only calls again — window only has
        // 4 entries, so the detector cannot trip yet (8 needed).
        for (tool, args) in &rotation {
            guard.register_progress(tool, args);
        }
        assert!(
            guard.last_trip_reason().is_none(),
            "terminal event should have cleared the window — but a trip fired"
        );
    }

    /// LoopGuardState roundtrip with the new fields: legacy snapshots
    /// (without the new fields in JSON) must restore cleanly via serde
    /// defaults.
    #[test]
    fn legacy_loop_guard_state_roundtrips_with_defaults() {
        let legacy_json = r#"{
            "max_loops_without_progress": 5,
            "max_tool_failures": 5,
            "max_consecutive_same_progress": 1,
            "max_child_failures": 3,
            "current_loops": 2,
            "tool_failure_counts": {},
            "last_progress_fingerprint": null,
            "consecutive_progress_count": 0,
            "child_failure_count": 0
        }"#;
        let state: LoopGuardState =
            serde_json::from_str(legacy_json).expect("legacy snapshot must parse");
        assert_eq!(state.max_window_size, default_rotation_window_size());
        assert_eq!(state.max_distinct_floor, default_rotation_distinct_floor());
        assert!(state.recent_fingerprints.is_empty());
        // And a fresh guard restored from that snapshot must be usable.
        let mut guard = LoopGuard::restore(state);
        assert!(guard.check_loop().is_ok());
    }

    /// Snapshot/restore preserves the rotation window contents so a guard
    /// resumed from a checkpoint mid-rotation can still trip on the next
    /// few calls.
    #[test]
    fn loop_guard_snapshot_preserves_rotation_window() {
        let mut guard = rotation_guard(6, 4);
        for tool in ["a", "b", "c", "a"].iter() {
            guard.register_progress(tool, "{}");
        }
        let snap = guard.snapshot();
        assert_eq!(snap.recent_fingerprints.len(), 4);
        let mut restored = LoopGuard::restore(snap);
        // Push two more distinct calls to fill the window — distinct count
        // should be 5 ("a","b","c","d","e"), above floor 4 → no trip.
        restored.register_progress("d", "{}");
        restored.register_progress("e", "{}");
        assert!(restored.last_trip_reason().is_none());
        // One more "a" → window has 6 entries ("b","c","a","d","e","a")
        // with 5 distinct → still above floor 4 → no trip.
        restored.register_progress("a", "{}");
        assert!(restored.last_trip_reason().is_none());
        // Now collapse to a small rotation: 3 distinct in a window of 6.
        // Pump the same 2-tool rotation until window has only 2 distinct.
        for _ in 0..6 {
            restored.register_progress("loop", "{}");
        }
        assert!(
            restored.last_trip_reason().is_some(),
            "rotation should have tripped after collapse"
        );
    }

    /// Trip-reason `code()` strings are stable identifiers consumed by the
    /// `loop_guard.tripped` causal event payload.
    #[test]
    fn trip_reason_codes_are_stable() {
        assert_eq!(
            LoopGuardTripReason::NoMeaningfulProgress { cycles: 5 }.code(),
            "no_meaningful_progress"
        );
        assert_eq!(
            LoopGuardTripReason::ToolFailureBudget {
                tool: "x".to_string(),
                failures: 1,
            }
            .code(),
            "tool_failure_budget"
        );
        assert_eq!(
            LoopGuardTripReason::RotatingPollingPattern {
                window_size: 8,
                distinct_count: 5,
                floor: 5,
            }
            .code(),
            "rotating_polling_pattern"
        );
        assert_eq!(
            LoopGuardTripReason::ChildFailureBudget { failures: 3 }.code(),
            "child_failure_budget"
        );
    }

    /// Each trip reason must attribute to the constitutional rule whose
    /// text describes it — these feed `enforced_rules` on the
    /// `loop_guard.tripped` event. Pinned so a rule renumber or a new
    /// reason can't silently mislabel the audit trail.
    #[test]
    fn trip_reason_rule_ids_are_pinned() {
        assert_eq!(
            LoopGuardTripReason::ToolFailureBudget {
                tool: "x".to_string(),
                failures: 1,
            }
            .rule_id(),
            "P-7.5"
        );
        assert_eq!(
            LoopGuardTripReason::NoMeaningfulProgress { cycles: 5 }.rule_id(),
            "P-7.7"
        );
        assert_eq!(
            LoopGuardTripReason::RotatingPollingPattern {
                window_size: 16,
                distinct_count: 6,
                floor: 6,
            }
            .rule_id(),
            "P-7.19"
        );
        assert_eq!(
            LoopGuardTripReason::ChildFailureBudget { failures: 3 }.rule_id(),
            "P-7.20"
        );
    }

    /// Disabling the detector via `rotation_window_size = 0` should let
    /// even pathological rotations through (useful for diagnostic or
    /// migration sessions).
    #[test]
    fn rotation_window_zero_disables_detector() {
        let mut guard = rotation_guard(0, 5);
        for _ in 0..40 {
            for tool in ["a", "b", "c"].iter() {
                guard.register_progress(tool, "{}");
            }
        }
        assert!(guard.last_trip_reason().is_none());
    }
}
