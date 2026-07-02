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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Trip condition #5 — a read-only roster tool (`agent_list` /
    /// `agent_inspect` / `agent_discover`) was called `repeats` times in a
    /// row with identical normalized arguments, reaching `floor`. These
    /// directory reads are idempotent, so a tight repeat means the agent is
    /// stuck looking for an input schema instead of spawning. Fires fast,
    /// before the generic rotating-polling window fills.
    RedundantRosterPolling {
        tool: String,
        repeats: u32,
        floor: u32,
    },
    /// Trip condition #6 — consecutive LLM endpoint failures.
    LlmFailureBudget { failures: u32 },
    /// Trip condition #7 — a tool returned a deterministic permanent failure
    /// (e.g. agent_spawn rejected because the bound workflow is already
    /// terminal). Retrying the same call can never succeed, so end the turn
    /// immediately instead of letting the agent burn its tool-failure budget.
    WorkflowTerminal { workflow_id: String },
    /// Trip condition #8 — the same normalized error fingerprint has surfaced
    /// from `distinct_tools.len()` different tool names within the recent
    /// window (issue #703). The agent is trying different approaches against
    /// one unrecoverable root cause — a pattern the per-tool failure budget
    /// misses because each tool's individual count stays low.
    RecurringUnrecoverableError {
        error_hash: u64,
        distinct_tools: Vec<String>,
        occurrences: u32,
    },
    /// Trip condition #9 — the same `(tool, normalized-error)` irrecoverable
    /// rejection (permission / quota / sandbox-unavailable) has recurred
    /// `occurrences` times (issue #718). These rejections are excluded from the
    /// per-tool failure budget because the agent cannot fix them by retrying
    /// with different arguments — but re-issuing the *identical* call and
    /// getting the *identical* deterministic answer is a no-progress loop
    /// (P-7.7). Unlike #8 this fires on a single tool re-hammering one gate
    /// (e.g. `agent_revision_promote` against a standing
    /// `capability_delta_requires_approval`), which the distinct-tools
    /// threshold of the recurring-error detector never sees.
    RepeatedIrrecoverableRejection {
        tool: String,
        error_hash: u64,
        occurrences: u32,
    },
}

impl LoopGuardTripReason {
    /// Stable identifier for causal event payloads.
    pub fn code(&self) -> &'static str {
        match self {
            LoopGuardTripReason::NoMeaningfulProgress { .. } => "no_meaningful_progress",
            LoopGuardTripReason::ToolFailureBudget { .. } => "tool_failure_budget",
            LoopGuardTripReason::RotatingPollingPattern { .. } => "rotating_polling_pattern",
            LoopGuardTripReason::ChildFailureBudget { .. } => "child_failure_budget",
            LoopGuardTripReason::RedundantRosterPolling { .. } => "redundant_roster_polling",
            LoopGuardTripReason::LlmFailureBudget { .. } => "llm_failure_budget",
            LoopGuardTripReason::WorkflowTerminal { .. } => "workflow_terminal",
            LoopGuardTripReason::RecurringUnrecoverableError { .. } => {
                "recurring_unrecoverable_error"
            }
            LoopGuardTripReason::RepeatedIrrecoverableRejection { .. } => {
                "repeated_irrecoverable_rejection"
            }
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
    /// - `RedundantRosterPolling`→ P-7.19 (no semantic progress across successes)
    /// - `LlmFailureBudget`       → P-7.5 (consecutive failures)
    /// - `WorkflowTerminal`       → P-7.5 (deterministic tool failure)
    /// - `RecurringUnrecoverableError` → P-7.7 (no progress across different tools)
    /// - `RepeatedIrrecoverableRejection` → P-7.7 (re-asking one answered gate)
    pub fn rule_id(&self) -> &'static str {
        match self {
            LoopGuardTripReason::ToolFailureBudget { .. } => "P-7.5",
            LoopGuardTripReason::NoMeaningfulProgress { .. } => "P-7.7",
            LoopGuardTripReason::RotatingPollingPattern { .. } => "P-7.19",
            LoopGuardTripReason::ChildFailureBudget { .. } => "P-7.20",
            LoopGuardTripReason::RedundantRosterPolling { .. } => "P-7.19",
            LoopGuardTripReason::LlmFailureBudget { .. } => "P-7.5",
            LoopGuardTripReason::WorkflowTerminal { .. } => "P-7.5",
            // Same "no progress despite trying different tools" family as
            // NoMeaningfulProgress.
            LoopGuardTripReason::RecurringUnrecoverableError { .. } => "P-7.7",
            // Re-asking one gate that already gave a deterministic answer is
            // the single-tool sibling of NoMeaningfulProgress.
            LoopGuardTripReason::RepeatedIrrecoverableRejection { .. } => "P-7.7",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoopGuard {
    pub max_loops_without_progress: u32,
    pub max_tool_failures: u32,
    pub max_consecutive_same_progress: u32,
    pub max_child_failures: u32,
    /// Trip condition #3 — recent-call window cap.
    #[serde(default = "default_rotation_window_size")]
    pub max_window_size: usize,
    /// Trip condition #3 — minimum distinct fingerprints required to clear.
    #[serde(default = "default_rotation_distinct_floor")]
    pub max_distinct_floor: usize,
    /// Trip condition #5 — consecutive identical read-only roster reads that
    /// trigger the fast-path `RedundantRosterPolling` trip. 0 disables it.
    #[serde(default = "default_roster_repeat_floor")]
    pub roster_repeat_floor: u32,
    /// Trip condition #6 — consecutive LLM transport/endpoint failures. Unlike
    /// tool failures, these are not per-tool: they count any failed LLM API
    /// call (HTTP error, timeout, connection refused). When this reaches
    /// `max_llm_failures`, the guard trips to prevent expensive retry spirals.
    #[serde(default)]
    pub llm_failure_count: u32,
    #[serde(default = "default_max_llm_failures")]
    pub max_llm_failures: u32,
    /// From gateway config — max loop resets attributable to each tool name.
    #[serde(default)]
    pub progress_budget_tools: HashMap<String, u32>,
    /// How many times each budgeted tool has reset `current_loops` this session.
    #[serde(default)]
    pub progress_budget_used: HashMap<String, u32>,
    pub current_loops: u32,
    #[serde(default)]
    pub tool_failure_counts: std::collections::HashMap<String, u32>,
    pub last_progress_fingerprint: Option<(String, u64)>,
    pub consecutive_progress_count: u32,
    pub child_failure_count: u32,
    /// Loop-counter penalty applied on each child failure (#704). A queued
    /// `agent_spawn` returns `ok: true` and resets `current_loops` via
    /// `register_progress`, but if that child later fails the spawn produced
    /// no net progress — so `register_child_failure` advances `current_loops`
    /// by this amount (it does NOT reset it). 0 restores the legacy behavior.
    #[serde(default = "default_child_failure_loop_penalty")]
    pub child_failure_loop_penalty: u32,
    /// Repair-loop-aware accounting (RFC D.4). When `repair_mode` is true,
    /// `check_loop` counts against the repair budget instead of
    /// `max_loops_without_progress`, so a healthy repair cycle does not trip
    /// the outer LoopGuard.
    #[serde(default)]
    pub repair_mode: bool,
    /// Loops consumed while inside a repair cycle.
    #[serde(default)]
    pub repair_loops: u32,
    /// Maximum loops allowed inside a single repair cycle.
    #[serde(default)]
    pub max_repair_loops: u32,
    /// Sliding window of fingerprint hashes for the last
    /// `max_window_size` successful tool calls. Used by trip condition #3
    /// (rotating-polling detector).
    #[serde(default)]
    pub recent_fingerprints: VecDeque<u64>,
    /// Sliding window of `(normalized_error_fingerprint, tool_name)` for the
    /// last `recurring_error_window` *error* tool-results. Trip condition #8
    /// (recurring-unrecoverable-error detector, issue #703).
    #[serde(default)]
    pub recent_error_fingerprints: VecDeque<(u64, String)>,
    /// Trip condition #8 — recent-error window size. 0 disables the detector.
    #[serde(default = "default_recurring_error_window")]
    pub recurring_error_window: usize,
    /// Trip condition #8 — distinct-tool threshold for the same error hash.
    #[serde(default = "default_recurring_error_distinct_tools")]
    pub recurring_error_distinct_tools: usize,
    /// Trip condition #9 — per-`(tool, normalized-error)` recurrence counts for
    /// irrecoverable rejections (issue #718). Keyed by `tool\0<error-hash>` so
    /// distinct rejections and distinct tools never share a counter.
    #[serde(default)]
    pub irrecoverable_repeat_counts: HashMap<String, u32>,
    /// Trip condition #9 — recurrence threshold. When a `(tool, error)` count
    /// reaches this, the guard trips `RepeatedIrrecoverableRejection`. 0
    /// disables the detector.
    #[serde(default = "default_max_irrecoverable_repeats")]
    pub max_irrecoverable_repeats: u32,
    /// Trip reason recorded when any condition fires. Cleared on construction
    /// and never reset — once a guard has tripped, subsequent calls are
    /// errors. `last_trip_reason` exposes this for causal-event emission.
    /// Not present in legacy `LoopGuard` snapshots; defaulted to `None`.
    #[serde(default)]
    pub trip_reason: Option<LoopGuardTripReason>,
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
            roster_repeat_floor: default_roster_repeat_floor(),
            llm_failure_count: 0,
            max_llm_failures: 3,
            progress_budget_tools: HashMap::new(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            child_failure_loop_penalty: default_child_failure_loop_penalty(),
            recent_fingerprints: VecDeque::new(),
            recent_error_fingerprints: VecDeque::new(),
            recurring_error_window: default_recurring_error_window(),
            recurring_error_distinct_tools: default_recurring_error_distinct_tools(),
            irrecoverable_repeat_counts: HashMap::new(),
            max_irrecoverable_repeats: default_max_irrecoverable_repeats(),
            trip_reason: None,
            repair_mode: false,
            repair_loops: 0,
            max_repair_loops: 0,
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
            roster_repeat_floor: cfg.roster_repeat_floor,
            llm_failure_count: 0,
            max_llm_failures: cfg.max_llm_failures,
            progress_budget_tools: cfg.progress_budget_tools.clone(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            child_failure_loop_penalty: cfg.child_failure_loop_penalty,
            recent_fingerprints: VecDeque::new(),
            recent_error_fingerprints: VecDeque::new(),
            recurring_error_window: cfg.recurring_error_window,
            recurring_error_distinct_tools: cfg.recurring_error_distinct_tools,
            irrecoverable_repeat_counts: HashMap::new(),
            max_irrecoverable_repeats: cfg.max_irrecoverable_repeats,
            trip_reason: None,
            repair_mode: false,
            repair_loops: 0,
            max_repair_loops: 0,
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

        // RFC D.4: repair-loop-aware accounting. While the session is inside a
        // response-validation repair cycle, count against the repair budget
        // instead of the outer `max_loops_without_progress`. Healthy repair
        // (distinct violations each iteration) should not trip the LoopGuard.
        if self.repair_mode {
            if self.max_repair_loops > 0 && self.repair_loops >= self.max_repair_loops {
                let reason = LoopGuardTripReason::NoMeaningfulProgress {
                    cycles: self.repair_loops,
                };
                self.trip_reason = Some(reason.clone());
                return Err(format_trip_error(&reason));
            }
            self.repair_loops += 1;
            return Ok(());
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

        if self.llm_failure_count >= self.max_llm_failures {
            let reason = LoopGuardTripReason::LlmFailureBudget {
                failures: self.llm_failure_count,
            };
            self.trip_reason = Some(reason.clone());
            return Err(format_trip_error(&reason));
        }

        self.current_loops += 1;
        Ok(())
    }

    /// Returns the trip reason if the guard has already tripped, so the
    /// caller can emit a structured causal event before propagating the trip
    /// as an error. Returns `None` when the guard is still healthy.
    pub fn last_trip_reason(&self) -> Option<&LoopGuardTripReason> {
        self.trip_reason.as_ref()
    }

    /// Hard-trip the guard immediately with the given reason. Used when a
    /// tool returns a deterministic permanent failure (e.g. spawning into a
    /// terminal workflow) so the agent stops instead of retrying forever.
    pub fn trip(&mut self, reason: LoopGuardTripReason) {
        self.trip_reason = Some(reason);
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

    /// Enter repair-loop-aware accounting. While in repair mode,
    /// `check_loop` does not increment `current_loops`; it increments
    /// `repair_loops` instead and enforces `max_repair_loops`.
    pub fn enter_repair_mode(&mut self, max_repair_loops: u32) {
        self.repair_mode = true;
        self.repair_loops = 0;
        self.max_repair_loops = max_repair_loops;
    }

    /// Exit repair mode. Preserves the outer `current_loops` so a failed
    /// repair does not erase legitimate progress pressure.
    pub fn exit_repair_mode(&mut self) {
        self.repair_mode = false;
        self.repair_loops = 0;
    }

    /// A successful repair resets both the outer loop counter and the repair
    /// window, because the agent just produced a valid output.
    pub fn reset_after_successful_repair(&mut self) {
        self.current_loops = 0;
        self.repair_mode = false;
        self.repair_loops = 0;
    }

    pub fn is_irrecoverable(error_type: &ToolErrorType) -> bool {
        matches!(
            error_type,
            ToolErrorType::Permission | ToolErrorType::QuotaExceeded | ToolErrorType::SandboxUnavailable
        )
    }

    /// Track a tool failure — failures accumulate per tool name regardless of arguments.
    ///
    /// Irrecoverable errors (permission, quota exceeded, sandbox unavailable) are
    /// excluded: the agent cannot fix them by retrying.
    pub fn register_failure(
        &mut self,
        tool_name: &str,
        _arguments: &str,
        error_type: Option<&ToolErrorType>,
    ) -> bool {
        if let Some(e) = error_type {
            if Self::is_irrecoverable(e) {
                return false;
            }
        }
        *self
            .tool_failure_counts
            .entry(tool_name.to_string())
            .or_insert(0) += 1;
        true
    }

    /// Track a child agent task failure (from workflow.wait returning any_failed: true).
    ///
    /// Also advances `current_loops` by `child_failure_loop_penalty` (#704): the
    /// `agent_spawn` that queued this child reset the no-progress counter when it
    /// returned `ok: true`, but a failed child means that spawn produced no net
    /// progress. This penalizes the loop counter (it does NOT reset it), so a
    /// spawn → probe → spawn → probe death spiral reaches
    /// `max_loops_without_progress` after a couple of child-failure cycles
    /// instead of never. `child_failure_count` (the separate P-7.20 budget) is
    /// still incremented and is unaffected by progress resets.
    pub fn register_child_failure(&mut self) {
        self.child_failure_count += 1;
        self.current_loops = self
            .current_loops
            .saturating_add(self.child_failure_loop_penalty);
    }

    /// Track an error tool-result for the recurring-unrecoverable-error
    /// detector (#703). Fingerprints the error (volatile ids/timestamps/numbers
    /// stripped) and records `(fingerprint, tool_name)` in a sliding window.
    /// Trips `RecurringUnrecoverableError` when the same fingerprint has been
    /// seen from at least `recurring_error_distinct_tools` distinct tool names
    /// within the window — the agent is hitting one unrecoverable root cause
    /// through different tools, which the per-tool failure budget cannot see.
    ///
    /// No-ops when the result carries no error, when the detector is disabled
    /// (`recurring_error_window == 0` or `recurring_error_distinct_tools < 2`),
    /// when the guard has already tripped, or while `repair_mode` is active
    /// (response-validation repair cycles already have their own bounded loop).
    pub fn register_error(&mut self, tool_name: &str, result_json: &str) {
        if self.recurring_error_window == 0
            || self.recurring_error_distinct_tools < 2
            || self.trip_reason.is_some()
            || self.repair_mode
        {
            return;
        }
        let Some(hash) = crate::runtime::error_fingerprint::fingerprint_result(result_json) else {
            return;
        };

        if self.recent_error_fingerprints.len() >= self.recurring_error_window {
            self.recent_error_fingerprints.pop_front();
        }
        self.recent_error_fingerprints
            .push_back((hash, tool_name.to_string()));

        // Distinct tool names that produced this same error hash in the window.
        let mut distinct: Vec<String> = Vec::new();
        let mut occurrences = 0u32;
        for (h, tool) in &self.recent_error_fingerprints {
            if *h == hash {
                occurrences += 1;
                if !distinct.iter().any(|t| t == tool) {
                    distinct.push(tool.clone());
                }
            }
        }

        if distinct.len() >= self.recurring_error_distinct_tools {
            self.trip_reason = Some(LoopGuardTripReason::RecurringUnrecoverableError {
                error_hash: hash,
                distinct_tools: distinct,
                occurrences,
            });
        }
    }

    /// Track an irrecoverable (gateway-side) tool rejection — a `permission` /
    /// `quota_exceeded` / `sandbox_unavailable` error (or a signal-derived
    /// exit) that [`register_failure`] deliberately excludes from the per-tool
    /// failure budget because retrying with different arguments cannot fix it.
    ///
    /// The first occurrences are free: a gateway-side block is not agent
    /// divergence, and the agent legitimately ends its turn to wait for an
    /// operator (e.g. an `agent_revision_promote` that returns
    /// `capability_delta_requires_approval`, or a network gate awaiting
    /// approval). But re-issuing the *same* call and getting the *same*
    /// deterministic rejection is a no-progress loop (P-7.7) — the agent
    /// re-asked a question the gateway already answered. When the same
    /// `(tool, normalized-error)` rejection recurs `max_irrecoverable_repeats`
    /// times the guard trips [`LoopGuardTripReason::RepeatedIrrecoverableRejection`].
    ///
    /// Distinct rejections never accumulate together: fixing one gate and
    /// hitting the next is progress, not a loop. The counter is keyed on the
    /// normalized error fingerprint (volatile ids/timestamps/numbers stripped)
    /// so cosmetic churn in the message doesn't defeat the match, and it rides
    /// in the checkpointed guard state so a post-approval resume that re-hits
    /// the identical rejection keeps counting across the suspend.
    ///
    /// No-ops when the detector is disabled (`max_irrecoverable_repeats == 0`),
    /// when the guard has already tripped, while `repair_mode` is active
    /// (response-validation repair cycles have their own bounded loop), or when
    /// the result carries no fingerprintable error text.
    pub fn register_irrecoverable(&mut self, tool_name: &str, result_json: &str) {
        if self.max_irrecoverable_repeats == 0 || self.trip_reason.is_some() || self.repair_mode {
            return;
        }
        let Some(hash) = crate::runtime::error_fingerprint::fingerprint_result(result_json) else {
            return;
        };
        let key = format!("{tool_name}\u{0}{hash:016x}");
        let count = self.irrecoverable_repeat_counts.entry(key).or_insert(0);
        *count += 1;
        if *count >= self.max_irrecoverable_repeats {
            self.trip_reason = Some(LoopGuardTripReason::RepeatedIrrecoverableRejection {
                tool: tool_name.to_string(),
                error_hash: hash,
                occurrences: *count,
            });
        }
    }

    /// Track an LLM transport/endpoint failure. Counts consecutively — a
    /// successful LLM call resets the counter to 0. Trips the guard at
    /// `max_llm_failures` to prevent expensive retry spirals against a
    /// flapping endpoint.
    pub fn register_llm_failure(&mut self) {
        self.llm_failure_count += 1;
    }

    /// Reset the LLM failure counter after a successful completion.
    pub fn register_llm_success(&mut self) {
        self.llm_failure_count = 0;
    }

    /// Track a successful tool call. Only counts as "progress" (resets current_loops)
    /// if this is a different tool call than the last successful one, or if the same
    /// tool+args has not repeated more than `max_consecutive_same_progress` times.
    /// This prevents agents from spinning on repeated identical successful calls
    /// (e.g., web.search returning the same cached results).
    pub fn register_progress(&mut self, tool_name: &str, arguments: &str) {
        self.register_progress_inner(tool_name, arguments, false, false);
    }

    /// Track a successful call to a read-only, side-effect-free tool (#701).
    /// Read-only probes (`resolve`, `workflow_state`, `planframe_get`,
    /// `approval_list`, `knowledge_search`, …) advance no workflow, so they must
    /// NOT reset `current_loops` — otherwise a planner can interleave one probe
    /// between every failed mutation and keep `max_loops_without_progress` from
    /// ever tripping. The rotating-polling window and roster fast-path are still
    /// updated (a read-only tool is the primary rotation candidate); only the
    /// `current_loops` reset is skipped.
    pub fn register_readonly_progress(&mut self, tool_name: &str, arguments: &str) {
        self.register_progress_inner(tool_name, arguments, false, true);
    }

    /// Track a successful tool call whose result carried
    /// `side_effect_state: "committed"` (P-5.14 / P-6.26). This is treated
    /// as terminal-progress evidence — the rotating-polling detector window
    /// is cleared because a real side effect just landed. The classical
    /// progress accounting (max_loops_without_progress / consecutive
    /// progress count) is otherwise unchanged.
    pub fn register_progress_terminal(&mut self, tool_name: &str, arguments: &str) {
        self.register_progress_inner(tool_name, arguments, true, false);
    }

    fn register_progress_inner(
        &mut self,
        tool_name: &str,
        arguments: &str,
        terminal_side_effect: bool,
        read_only: bool,
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

        // Trip condition #5: redundant roster polling (fast path).
        //
        // Read-only roster reads are idempotent — re-listing never returns new
        // data. When the agent repeats the same one `roster_repeat_floor`
        // times in a row it is stuck (typically hunting for an input schema
        // that does not exist for reasoning agents). Trip immediately with a
        // corrective reason rather than waiting for the 16-call rotating
        // window. Only set on a *repeat* (not the first call) and don't
        // overwrite a previously latched reason.
        if self.roster_repeat_floor > 0
            && !is_new
            && is_roster_read_tool(tool_name)
            && self.consecutive_progress_count >= self.roster_repeat_floor
            && self.trip_reason.is_none()
        {
            self.trip_reason = Some(LoopGuardTripReason::RedundantRosterPolling {
                tool: tool_name.to_string(),
                repeats: self.consecutive_progress_count,
                floor: self.roster_repeat_floor,
            });
        }

        // Read-only tools never reset the no-progress counter (#701). The
        // fingerprint window and roster fast-path above still ran, so rotating
        // read-only polling is still caught — we only skip the `current_loops`
        // reset and its budget bookkeeping.
        let would_reset_loops = !read_only
            && (is_new || self.consecutive_progress_count <= self.max_consecutive_same_progress);

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

    pub fn snapshot(&self) -> LoopGuard {
        self.clone()
    }

    pub fn restore(state: LoopGuard) -> Self {
        state
    }
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self {
            max_loops_without_progress: 10,
            max_tool_failures: 8,
            max_consecutive_same_progress: 1,
            max_child_failures: 5,
            max_window_size: default_rotation_window_size(),
            max_distinct_floor: default_rotation_distinct_floor(),
            roster_repeat_floor: default_roster_repeat_floor(),
            llm_failure_count: 0,
            max_llm_failures: default_max_llm_failures(),
            progress_budget_tools: HashMap::new(),
            progress_budget_used: HashMap::new(),
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
            child_failure_loop_penalty: default_child_failure_loop_penalty(),
            recent_fingerprints: VecDeque::new(),
            recent_error_fingerprints: VecDeque::new(),
            recurring_error_window: default_recurring_error_window(),
            recurring_error_distinct_tools: default_recurring_error_distinct_tools(),
            irrecoverable_repeat_counts: HashMap::new(),
            max_irrecoverable_repeats: default_max_irrecoverable_repeats(),
            trip_reason: None,
            repair_mode: false,
            repair_loops: 0,
            max_repair_loops: 0,
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
        LoopGuardTripReason::RedundantRosterPolling {
            tool,
            repeats,
            floor,
        } => anyhow::anyhow!(
            "LoopGuard tripped: '{}' was called {} times in a row with the same \
             arguments (floor {}). Roster directory reads are idempotent — you \
             already have this information and re-listing will not add fields. \
             Reasoning agents (researcher/architect/coder/etc.) take a free-form \
             natural-language `message`: call agent.spawn directly with the \
             agent_id and a plain-text task. A null `io_accepts` (message_format \
             \"free_text\") is expected, not missing data. If you are missing an \
             operator decision instead, use user.ask or end the turn.",
            tool,
            repeats,
            floor
        ),
        LoopGuardTripReason::LlmFailureBudget { failures } => anyhow::anyhow!(
            "LoopGuard tripped: {} consecutive LLM endpoint failures. \
             The model API is unavailable — suspending to prevent retry spirals.",
            failures
        ),
        LoopGuardTripReason::WorkflowTerminal { workflow_id } => anyhow::anyhow!(
            "LoopGuard tripped: workflow {} is already terminal. \
             Cannot spawn new tasks against it — resume or start a new workflow.",
            workflow_id
        ),
        LoopGuardTripReason::RecurringUnrecoverableError {
            distinct_tools,
            occurrences,
            ..
        } => anyhow::anyhow!(
            "LoopGuard tripped: the same error recurred {} times across {} different \
             tools ({}) — this root cause is unrecoverable by retrying or switching \
             tools. Escalate to the operator or change strategy (issue #703).",
            occurrences,
            distinct_tools.len(),
            distinct_tools.join(", ")
        ),
        LoopGuardTripReason::RepeatedIrrecoverableRejection {
            tool,
            occurrences,
            ..
        } => anyhow::anyhow!(
            "LoopGuard tripped: '{}' returned the same irrecoverable rejection {} \
             times. This is a gateway-side gate (e.g. an approval requirement or a \
             permission denial), not a fixable error — re-issuing the identical call \
             gets the identical answer. If you are waiting on an operator approval, \
             end your turn: the gateway resumes the session when the decision lands. \
             Otherwise change what you are asking for or escalate (issue #718).",
            tool,
            occurrences
        ),
    }
}

fn default_rotation_window_size() -> usize {
    16
}

fn default_rotation_distinct_floor() -> usize {
    6
}

fn default_roster_repeat_floor() -> u32 {
    3
}

fn default_max_llm_failures() -> u32 {
    3
}

fn default_child_failure_loop_penalty() -> u32 {
    2
}

fn default_recurring_error_window() -> usize {
    10
}

fn default_recurring_error_distinct_tools() -> usize {
    3
}

fn default_max_irrecoverable_repeats() -> u32 {
    3
}

/// Read-only roster directory tools whose results are idempotent: repeating
/// the same call never surfaces new data. Used by the fast-path
/// `RedundantRosterPolling` trip so a stuck spawn loop breaks in a few calls
/// instead of waiting for the generic rotating-polling window to fill.
fn is_roster_read_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "agent_list" | "agent_inspect" | "agent_discover"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_terminal_trip_reason_formats_and_rules() {
        let reason = LoopGuardTripReason::WorkflowTerminal {
            workflow_id: "wf-test".to_string(),
        };
        assert_eq!(reason.code(), "workflow_terminal");
        assert_eq!(reason.rule_id(), "P-7.5");

        let mut guard = LoopGuard::new(10);
        guard.trip(reason.clone());
        let err = guard.check_loop().expect_err("guard must trip");
        let msg = format!("{err:#}");
        assert!(msg.contains("wf-test"), "message must mention workflow id: {msg}");
        assert!(msg.contains("already terminal"), "message must mention terminal state: {msg}");
    }

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
        // Isolate the intent-normalization path from the roster fast-path
        // (roster_repeat_floor: 0) so this test exercises the generic
        // NoMeaningfulProgress accounting on a tool whose only varying field
        // is the echoed `intent`. agent_inspect's roster fast-path is covered
        // separately by `roster_polling_trips_fast`.
        let cfg = autonoetic_types::config::LoopGuardConfig {
            max_loops_without_progress: 5,
            roster_repeat_floor: 0,
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
                "agent_inspect",
                &format!(
                    r#"{{"agent_id":"weather.default","intent":"check-{epoch}"}}"#
                ),
            );
        }
        unreachable!("check_loop did not trip");
    }

    /// Fast-path trip (P-7.19): repeated read-only roster reads with the same
    /// normalized arguments trip `RedundantRosterPolling` at
    /// `roster_repeat_floor` (default 3) — well before the generic 16-call
    /// rotating-polling window could fill. Regression for the
    /// planner.collaborative `agent_list {}` spin observed after the move to
    /// the collaborative tool tier.
    #[test]
    fn roster_polling_trips_fast() {
        // High generic budgets so the roster fast-path is unambiguously the
        // trip cause, not NoMeaningfulProgress.
        let cfg = autonoetic_types::config::LoopGuardConfig {
            max_loops_without_progress: 100,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);

        // Vary only the echoed intent — normalization collapses these to one
        // fingerprint, mirroring the observed loop.
        assert!(guard.check_loop().is_ok());
        guard.register_progress("agent_list", r#"{"intent":"find researcher"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress("agent_list", r#"{"intent":"check io schema"}"#);
        // Third identical call reaches the floor → next check_loop trips.
        assert!(guard.check_loop().is_ok());
        guard.register_progress("agent_list", r#"{}"#);

        let err = guard.check_loop().expect_err("roster polling should trip");
        assert!(
            err.to_string().contains("agent_list"),
            "trip error should name the tool: {err}"
        );
        assert!(matches!(
            guard.last_trip_reason(),
            Some(LoopGuardTripReason::RedundantRosterPolling { floor: 3, .. })
        ));
        assert_eq!(guard.last_trip_reason().unwrap().rule_id(), "P-7.19");
    }

    /// A single roster read followed by real work must NOT trip — the fast
    /// path only fires on consecutive identical repeats.
    #[test]
    fn single_roster_read_then_spawn_does_not_trip() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());
        guard.register_progress("agent_list", r#"{}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress("agent_spawn", r#"{"agent_id":"researcher.default"}"#);
        assert!(guard.check_loop().is_ok());
        assert!(guard.last_trip_reason().is_none());
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
    /// (default 5, raised from 3 in ff87497) is reached. Pinning test cited by
    /// the enforcement register for P-7 / P-7.20.
    #[test]
    fn test_loop_guard_trips_on_child_failures() {
        let mut guard = LoopGuard::new(100); // high loop budget so the child budget is the trip cause
        for _ in 0..4 {
            guard.register_child_failure();
        }
        assert!(guard.check_loop().is_ok(), "4 < default max_child_failures(5)");
        guard.register_child_failure(); // now 5
        let err = guard.check_loop().expect_err("5 >= max_child_failures must trip");
        assert!(err.to_string().contains("child"), "unexpected error: {err}");
        assert!(matches!(
            guard.last_trip_reason(),
            Some(crate::runtime::guard::LoopGuardTripReason::ChildFailureBudget { .. })
        ));
    }

    /// #701: a read-only probe must NOT reset `current_loops`. Without the fix,
    /// interleaving one probe between every no-op turn keeps the guard from
    /// ever tripping. With it, `register_readonly_progress` leaves the counter
    /// climbing so `max_loops_without_progress` still fires.
    #[test]
    fn readonly_progress_does_not_reset_loop_counter() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok()); // current_loops -> 1
        assert!(guard.check_loop().is_ok()); // -> 2
        // A read-only probe returns ok but must not reset the counter.
        guard.register_readonly_progress("workflow_state", r#"{}"#);
        assert!(guard.check_loop().is_ok()); // -> 3
        assert!(
            guard.check_loop().is_err(),
            "read-only progress must not have reset current_loops"
        );
    }

    /// Roster directory reads (`agent_list`, `agent_discover`) are idempotent
    /// read-only probes and must also skip the `current_loops` reset (#701).
    /// They are handled by the same `register_readonly_progress` path, so a
    /// planner cannot use them to keep the no-progress counter pinned.
    #[test]
    fn roster_directory_reads_do_not_reset_loop_counter() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok()); // current_loops -> 1
        assert!(guard.check_loop().is_ok()); // -> 2
        guard.register_readonly_progress("agent_list", r#"{}"#); // still 2
        assert!(guard.check_loop().is_ok()); // -> 3
        guard.register_readonly_progress("agent_discover", r#"{"intent":"find researcher"}"#); // still 3
        assert!(
            guard.check_loop().is_err(), // -> 4
            "roster directory reads must not reset current_loops"
        );
    }

    /// Sanity contrast: a genuine (non-read-only) success DOES reset the
    /// counter, so the read-only carve-out is what changes behavior.
    #[test]
    fn mutating_progress_still_resets_loop_counter() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        guard.register_progress("content_write", r#"{"name":"out","content":"x"}"#);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_err());
    }

    /// Read-only rotation is still caught by the rotating-polling detector —
    /// the fingerprint window keeps filling even though `current_loops` never
    /// resets. Guards against the carve-out silently disabling detector #3.
    #[test]
    fn readonly_progress_still_feeds_rotation_detector() {
        let mut guard = rotation_guard(8, 5);
        let rotation = [
            ("workflow_state", r#"{}"#),
            ("planframe_get", r#"{}"#),
            ("approval_list", r#"{}"#),
            ("promotion_query", r#"{}"#),
            ("resolve", r#"{"name":"f"}"#),
        ];
        for _ in 0..3 {
            for (tool, args) in &rotation {
                guard.register_readonly_progress(tool, args);
                if guard.last_trip_reason().is_some() {
                    assert!(matches!(
                        guard.last_trip_reason().unwrap(),
                        LoopGuardTripReason::RotatingPollingPattern { .. }
                    ));
                    return;
                }
            }
        }
        panic!("rotating-polling detector should trip on read-only rotation");
    }

    /// #704: a child failure penalizes `current_loops` (default penalty 2) in
    /// addition to bumping `child_failure_count`. This makes a spawn→probe
    /// death spiral trip `NoMeaningfulProgress` even when the child budget
    /// hasn't been reached.
    #[test]
    fn child_failure_penalizes_loop_counter() {
        let mut guard = LoopGuard::new(5);
        assert_eq!(guard.child_failure_loop_penalty, 2);
        // A spawn queued ok and reset progress.
        guard.register_progress("agent_spawn", r#"{"agent_id":"builder.default"}"#);
        assert_eq!(guard.current_loops, 0);
        // The child later fails: +2 to current_loops, +1 to child_failure_count.
        guard.register_child_failure();
        assert_eq!(guard.current_loops, 2);
        assert_eq!(guard.child_failure_count, 1);
        guard.register_child_failure(); // current_loops -> 4
        assert_eq!(guard.current_loops, 4);
        // 4 < 5 loop budget and 2 < 5 child budget, so still ok...
        assert!(guard.check_loop().is_ok()); // check_loop bumps to 5
        // ...next check_loop trips on NoMeaningfulProgress, not child budget.
        let err = guard.check_loop().expect_err("loop budget should trip");
        assert!(err.to_string().contains("meaningful progress"), "{err}");
    }

    /// The penalty is configurable; 0 restores legacy behavior (child failure
    /// only touches `child_failure_count`).
    #[test]
    fn child_failure_penalty_zero_is_legacy_behavior() {
        let cfg = autonoetic_types::config::LoopGuardConfig {
            child_failure_loop_penalty: 0,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);
        guard.register_child_failure();
        assert_eq!(guard.current_loops, 0, "penalty 0 must not touch current_loops");
        assert_eq!(guard.child_failure_count, 1);
    }

    /// #703: the same normalized error surfacing from 3 distinct tools trips
    /// `RecurringUnrecoverableError`, even though no single tool reached its
    /// failure budget.
    #[test]
    fn recurring_error_across_distinct_tools_trips() {
        let mut guard = LoopGuard::new(100); // high loop budget: recurring-error is the cause
        let err = |id: &str| {
            format!(
                r#"{{"ok":false,"error":"workflow wf-{id} was reactivated and cannot accept child-session spawns"}}"#
            )
        };
        // Same root cause (volatile wf id differs) from three different tools.
        guard.register_error("agent_spawn", &err("aaa111"));
        assert!(guard.last_trip_reason().is_none(), "1 tool: no trip");
        guard.register_error("agent_revision_create_from_intent", &err("bbb222"));
        assert!(guard.last_trip_reason().is_none(), "2 tools: no trip");
        guard.register_error("workflow_wait", &err("ccc333"));

        let err_ = guard.check_loop().expect_err("3 distinct tools must trip");
        assert!(err_.to_string().contains("across 3 different tools"), "{err_}");
        assert!(matches!(
            guard.last_trip_reason(),
            Some(LoopGuardTripReason::RecurringUnrecoverableError { .. })
        ));
        assert_eq!(guard.last_trip_reason().unwrap().rule_id(), "P-7.7");
    }

    /// The same error from the SAME tool repeatedly does not trip #703 (that's
    /// the per-tool failure budget's job) — distinct tools is the trigger.
    #[test]
    fn recurring_error_same_tool_does_not_trip_recurring_detector() {
        let mut guard = LoopGuard::new(100);
        for _ in 0..5 {
            guard.register_error("agent_spawn", r#"{"ok":false,"error":"disk full"}"#);
        }
        assert!(
            !matches!(
                guard.last_trip_reason(),
                Some(LoopGuardTripReason::RecurringUnrecoverableError { .. })
            ),
            "same-tool repetition must not trip the cross-tool detector"
        );
    }

    /// Distinct errors across tools do not trip — only a shared fingerprint does.
    #[test]
    fn distinct_errors_across_tools_do_not_trip() {
        let mut guard = LoopGuard::new(100);
        guard.register_error("agent_spawn", r#"{"ok":false,"error":"disk full"}"#);
        guard.register_error("workflow_wait", r#"{"ok":false,"error":"permission denied"}"#);
        guard.register_error("planframe_get", r#"{"ok":false,"error":"not found"}"#);
        assert!(guard.last_trip_reason().is_none());
    }

    /// `recurring_error_window: 0` disables the detector.
    #[test]
    fn recurring_error_window_zero_disables_detector() {
        let cfg = autonoetic_types::config::LoopGuardConfig {
            recurring_error_window: 0,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);
        for tool in ["a", "b", "c", "d"] {
            guard.register_error(tool, r#"{"ok":false,"error":"same error"}"#);
        }
        assert!(guard.last_trip_reason().is_none());
    }

    /// `recurring_error_distinct_tools: 0` (or 1) disables the detector because
    /// the whole point is recurrence across *distinct* tools.
    #[test]
    fn recurring_error_distinct_tools_below_two_disables_detector() {
        let cfg = autonoetic_types::config::LoopGuardConfig {
            recurring_error_distinct_tools: 0,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);
        for tool in ["a", "b", "c", "d"] {
            guard.register_error(tool, r#"{"ok":false,"error":"same error"}"#);
        }
        assert!(guard.last_trip_reason().is_none());
    }

    /// Repair mode suppresses the recurring-error detector so response-
    /// validation repair cycles do not trip the outer guard.
    #[test]
    fn recurring_error_detector_noops_in_repair_mode() {
        let mut guard = LoopGuard::new(100);
        guard.enter_repair_mode(10);
        for tool in ["a", "b", "c"] {
            guard.register_error(tool, r#"{"ok":false,"error":"same error"}"#);
        }
        assert!(guard.last_trip_reason().is_none());
    }

    // ── #718: repeated-irrecoverable-rejection detector ──────────────────

    /// A stable promote-gate rejection re-hammered by a single tool trips
    /// `RepeatedIrrecoverableRejection` at the configured threshold — the
    /// per-tool failure budget never sees it (permission errors are excluded)
    /// and the cross-tool #703 detector never sees it (one tool). High loop
    /// budget so this detector is unambiguously the cause.
    #[test]
    fn repeated_irrecoverable_rejection_trips_on_same_tool() {
        let mut guard = LoopGuard::new(100);
        // request_id churns each attempt; the fingerprint must normalize it out.
        let reject = |req: &str| {
            format!(
                r#"{{"ok":false,"error_type":"permission","error":"capability_delta_requires_approval","request_id":"apr-{req}"}}"#
            )
        };
        // Default threshold is 3: two free re-asks, trip on the third.
        guard.register_irrecoverable("agent_revision_promote", &reject("aaa111"));
        assert!(guard.last_trip_reason().is_none(), "1st: free");
        guard.register_irrecoverable("agent_revision_promote", &reject("bbb222"));
        assert!(guard.last_trip_reason().is_none(), "2nd: free");
        guard.register_irrecoverable("agent_revision_promote", &reject("ccc333"));

        let err = guard.check_loop().expect_err("3rd identical rejection must trip");
        assert!(err.to_string().contains("irrecoverable rejection"), "{err}");
        assert!(matches!(
            guard.last_trip_reason(),
            Some(LoopGuardTripReason::RepeatedIrrecoverableRejection { occurrences: 3, .. })
        ));
        assert_eq!(guard.last_trip_reason().unwrap().rule_id(), "P-7.7");
        assert_eq!(
            guard.last_trip_reason().unwrap().code(),
            "repeated_irrecoverable_rejection"
        );
    }

    /// Distinct rejections never accumulate together: clearing one gate and
    /// hitting the next is progress, not a loop. Two different errors on the
    /// same tool each keep their own counter and neither reaches the threshold.
    #[test]
    fn repeated_irrecoverable_distinct_errors_do_not_accumulate() {
        let mut guard = LoopGuard::new(100);
        let t = "agent_revision_promote";
        guard.register_irrecoverable(t, r#"{"ok":false,"error":"capability_delta_requires_approval"}"#);
        guard.register_irrecoverable(t, r#"{"ok":false,"error":"auditor_pass_missing"}"#);
        guard.register_irrecoverable(t, r#"{"ok":false,"error":"jury_escalation_required"}"#);
        assert!(
            guard.last_trip_reason().is_none(),
            "three DIFFERENT gates is forward progress, not a loop"
        );
    }

    /// The counter is per-tool: the same error text from two different tools
    /// does not merge (that cross-tool case is #703's job, which needs 3).
    #[test]
    fn repeated_irrecoverable_is_scoped_per_tool() {
        let mut guard = LoopGuard::new(100);
        let e = r#"{"ok":false,"error":"permission denied for host"}"#;
        guard.register_irrecoverable("web_fetch", e);
        guard.register_irrecoverable("sandbox_exec", e);
        assert!(
            guard.last_trip_reason().is_none(),
            "one hit on each of two tools must not trip the single-tool detector"
        );
    }

    /// `max_irrecoverable_repeats: 0` disables the detector entirely.
    #[test]
    fn repeated_irrecoverable_zero_threshold_disables() {
        let cfg = autonoetic_types::config::LoopGuardConfig {
            max_irrecoverable_repeats: 0,
            ..Default::default()
        };
        let mut guard = LoopGuard::with_config(&cfg);
        for _ in 0..10 {
            guard.register_irrecoverable(
                "agent_revision_promote",
                r#"{"ok":false,"error":"capability_delta_requires_approval"}"#,
            );
        }
        assert!(guard.last_trip_reason().is_none());
    }

    /// Repair mode suppresses the detector (repair cycles have their own bound).
    #[test]
    fn repeated_irrecoverable_noops_in_repair_mode() {
        let mut guard = LoopGuard::new(100);
        guard.enter_repair_mode(10);
        for _ in 0..5 {
            guard.register_irrecoverable(
                "agent_revision_promote",
                r#"{"ok":false,"error":"capability_delta_requires_approval"}"#,
            );
        }
        assert!(guard.last_trip_reason().is_none());
    }

    /// The recurrence count rides in the checkpointed guard state, so a
    /// post-approval resume that re-hits the identical rejection keeps counting
    /// across the suspend rather than starting fresh (the exact scenario #718
    /// targets: approve → resume → re-issue → same gate again).
    #[test]
    fn repeated_irrecoverable_count_survives_serde_roundtrip() {
        let mut guard = LoopGuard::new(100);
        let reject = r#"{"ok":false,"error":"capability_delta_requires_approval"}"#;
        guard.register_irrecoverable("agent_revision_promote", reject);
        guard.register_irrecoverable("agent_revision_promote", reject);
        assert!(guard.last_trip_reason().is_none(), "2 of 3 before suspend");

        // Simulate checkpoint persist + restore.
        let json = serde_json::to_string(&guard).expect("serialize guard");
        let mut resumed: LoopGuard = serde_json::from_str(&json).expect("deserialize guard");

        resumed.register_irrecoverable("agent_revision_promote", reject);
        assert!(
            matches!(
                resumed.last_trip_reason(),
                Some(LoopGuardTripReason::RepeatedIrrecoverableRejection { occurrences: 3, .. })
            ),
            "count must persist across the suspend and trip on resume"
        );
    }

    /// A legacy checkpoint predating #718 (no `max_irrecoverable_repeats` /
    /// `irrecoverable_repeat_counts` fields) still deserializes, and the field
    /// defaults enable the detector rather than silently disabling it.
    #[test]
    fn repeated_irrecoverable_legacy_checkpoint_defaults_enabled() {
        // A minimal guard snapshot with none of the #718 fields present.
        let legacy = r#"{
            "max_loops_without_progress": 10,
            "max_tool_failures": 8,
            "max_consecutive_same_progress": 1,
            "max_child_failures": 5,
            "current_loops": 0,
            "last_progress_fingerprint": null,
            "consecutive_progress_count": 0,
            "child_failure_count": 0
        }"#;
        let mut guard: LoopGuard =
            serde_json::from_str(legacy).expect("legacy snapshot must deserialize");
        assert_eq!(guard.max_irrecoverable_repeats, 3, "field defaults to 3");
        let reject = r#"{"ok":false,"error":"capability_delta_requires_approval"}"#;
        for _ in 0..3 {
            guard.register_irrecoverable("agent_revision_promote", reject);
        }
        assert!(matches!(
            guard.last_trip_reason(),
            Some(LoopGuardTripReason::RepeatedIrrecoverableRejection { .. })
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
    fn test_is_irrecoverable() {
        assert!(LoopGuard::is_irrecoverable(&ToolErrorType::Permission));
        assert!(LoopGuard::is_irrecoverable(&ToolErrorType::QuotaExceeded));
        assert!(LoopGuard::is_irrecoverable(&ToolErrorType::SandboxUnavailable));
        assert!(!LoopGuard::is_irrecoverable(&ToolErrorType::Timeout));
        assert!(!LoopGuard::is_irrecoverable(&ToolErrorType::Resource));
    }

    #[test]
    fn test_irrecoverable_errors_do_not_count_against_budget() {
        let mut guard = LoopGuard::new(100);

        for _ in 0..10 {
            guard.register_failure(
                "web_fetch",
                r#"{"url":"https://denied.com"}"#,
                Some(&ToolErrorType::Permission),
            );
            guard.register_failure(
                "sandbox_exec",
                r#"{"command":"test"}"#,
                Some(&ToolErrorType::QuotaExceeded),
            );
            guard.register_failure(
                "sandbox_exec",
                r#"{"command":"test"}"#,
                Some(&ToolErrorType::SandboxUnavailable),
            );
            guard.register_progress("web_fetch", r#"{"url":"https://denied.com"}"#);
            assert!(guard.check_loop().is_ok());
        }
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

        guard.register_progress("resolve", r#"{"name":"file.txt"}"#);
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

    /// LoopGuard roundtrip with legacy snapshots (without the newer fields in
    /// JSON) must restore cleanly via serde defaults.
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
        let state: LoopGuard =
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
        assert_eq!(
            LoopGuardTripReason::RepeatedIrrecoverableRejection {
                tool: "agent_revision_promote".to_string(),
                error_hash: 0,
                occurrences: 3,
            }
            .code(),
            "repeated_irrecoverable_rejection"
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
        // #718: single-tool re-ask of an answered gate is the P-7.7 family.
        assert_eq!(
            LoopGuardTripReason::RepeatedIrrecoverableRejection {
                tool: "agent_revision_promote".to_string(),
                error_hash: 0,
                occurrences: 3,
            }
            .rule_id(),
            "P-7.7"
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

    #[test]
    fn llm_failures_trip_guard() {
        let mut guard = LoopGuard::new(10);
        guard.register_llm_failure();
        guard.register_llm_failure();
        assert!(guard.check_loop().is_ok(), "2 failures should not trip (max=3)");
        guard.register_llm_failure();
        let err = guard.check_loop().unwrap_err();
        assert!(err.to_string().contains("LLM endpoint failures"));
        assert_eq!(
            guard.last_trip_reason().unwrap().code(),
            "llm_failure_budget"
        );
    }

    #[test]
    fn llm_success_resets_failure_counter() {
        let mut guard = LoopGuard::new(10);
        guard.register_llm_failure();
        guard.register_llm_failure();
        guard.register_llm_success();
        guard.register_llm_failure();
        assert!(
            guard.check_loop().is_ok(),
            "counter should be 1 after reset + 1 failure"
        );
    }

    // ──────────────────────────────────────────────────────────────────────
    // RFC D.4 — repair-loop-aware accounting
    // ──────────────────────────────────────────────────────────────────────

    #[test]
    fn repair_mode_does_not_increment_current_loops() {
        let mut guard = LoopGuard::new(3);
        guard.enter_repair_mode(10);
        for _ in 0..5 {
            assert!(guard.check_loop().is_ok());
        }
        assert_eq!(guard.current_loops, 0, "current_loops must not grow in repair mode");
        assert_eq!(guard.repair_loops, 5, "repair_loops should track repair iterations");
    }

    #[test]
    fn repair_mode_enforces_max_repair_loops() {
        let mut guard = LoopGuard::new(100);
        guard.enter_repair_mode(2);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_err(), "repair should trip at max_repair_loops");
        assert!(matches!(
            guard.last_trip_reason(),
            Some(LoopGuardTripReason::NoMeaningfulProgress { .. })
        ));
    }

    #[test]
    fn reset_after_successful_repair_clears_loops() {
        let mut guard = LoopGuard::new(5);
        guard.current_loops = 4;
        guard.enter_repair_mode(10);
        guard.check_loop().unwrap();
        guard.reset_after_successful_repair();
        assert_eq!(guard.current_loops, 0);
        assert!(!guard.repair_mode);
        assert_eq!(guard.repair_loops, 0);
    }

    #[test]
    fn exit_repair_mode_preserves_outer_loops() {
        let mut guard = LoopGuard::new(5);
        guard.current_loops = 4;
        guard.enter_repair_mode(10);
        guard.check_loop().unwrap();
        guard.exit_repair_mode();
        assert_eq!(guard.current_loops, 4, "outer current_loops must be preserved");
        assert!(!guard.repair_mode);
        assert_eq!(guard.repair_loops, 0);
    }
}
