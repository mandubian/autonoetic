//! Loop Guard Mechanism.
//!
//! Prevents agents from getting stuck in infinite reasoning loops
//! without making progress.
//!
//! Two independent trip conditions:
//! 1. **Max loops without progress**: Agent executed N cycles without any
//!    *meaningful* tool call resetting the counter.
//! 2. **Tool failure budget exhausted**: A single tool has failed more than
//!    `max_tool_failures` times total in the session, regardless of arguments
//!    or targets. This catches alternating-failure patterns.
//!
//! "Meaningful progress" is determined by a fingerprint of the last
//! successful tool call. Repeated calls with the same (tool, arguments)
//! hash do not count as new progress — the agent is spinning on the same
//! operation. A call is considered "repeated" after
//! `max_consecutive_same_progress` consecutive occurrences.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use autonoetic_types::config::LoopGuardConfig;
use autonoetic_types::tool_error::ToolErrorType;

pub struct LoopGuard {
    max_loops_without_progress: u32,
    max_tool_failures: u32,
    max_consecutive_same_progress: u32,
    max_child_failures: u32,
    current_loops: u32,
    tool_failure_counts: std::collections::HashMap<String, u32>,
    last_progress_fingerprint: Option<(String, u64)>,
    consecutive_progress_count: u32,
    child_failure_count: u32,
}

impl LoopGuard {
    pub fn new(max_loops_without_progress: u32) -> Self {
        Self {
            max_loops_without_progress,
            max_tool_failures: 5,
            max_consecutive_same_progress: 1,
            max_child_failures: 3,
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
        }
    }

    pub fn with_config(cfg: &LoopGuardConfig) -> Self {
        Self {
            max_loops_without_progress: cfg.max_loops_without_progress,
            max_tool_failures: cfg.max_tool_failures,
            max_consecutive_same_progress: cfg.max_consecutive_same_progress,
            max_child_failures: cfg.max_child_failures,
            current_loops: 0,
            tool_failure_counts: std::collections::HashMap::new(),
            last_progress_fingerprint: None,
            consecutive_progress_count: 0,
            child_failure_count: 0,
        }
    }

    pub fn check_loop(&mut self) -> anyhow::Result<()> {
        if self.current_loops >= self.max_loops_without_progress {
            anyhow::bail!(
                "LoopGuard tripped: Agent executed {} cycles without meaningful progress.",
                self.current_loops
            );
        }

        for (tool_name, count) in &self.tool_failure_counts {
            if *count >= self.max_tool_failures {
                anyhow::bail!(
                    "LoopGuard tripped: Tool '{}' has failed {} times in this session. \
                     Breaking loop to prevent resource waste.",
                    tool_name,
                    count
                );
            }
        }

        if self.child_failure_count >= self.max_child_failures {
            anyhow::bail!(
                "LoopGuard tripped: {} child agent tasks have failed in this session. \
                 Breaking delegation loop — escalate to human or change strategy.",
                self.child_failure_count
            );
        }

        self.current_loops += 1;
        Ok(())
    }

    /// Track a tool failure — failures accumulate per tool name regardless of arguments.
    ///
    /// Permission errors are excluded from the budget: the agent cannot fix
    /// them by retrying with different arguments, so counting them would
    /// unfairly exhaust the budget and abort the session prematurely.
    pub fn register_failure(&mut self, tool_name: &str, _arguments: &str, error_type: Option<&ToolErrorType>) {
        if let Some(ToolErrorType::Permission) = error_type {
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
        let fp = compute_fingerprint(tool_name, arguments);
        let is_new = self.last_progress_fingerprint.as_ref() != Some(&fp);

        if is_new {
            self.consecutive_progress_count = 1;
        } else {
            self.consecutive_progress_count += 1;
        }

        if is_new || self.consecutive_progress_count <= self.max_consecutive_same_progress {
            self.current_loops = 0;
        }

        self.last_progress_fingerprint = Some(fp);
    }

    pub fn snapshot(&self) -> LoopGuardState {
        LoopGuardState {
            max_loops_without_progress: self.max_loops_without_progress,
            max_tool_failures: self.max_tool_failures,
            max_consecutive_same_progress: self.max_consecutive_same_progress,
            max_child_failures: self.max_child_failures,
            current_loops: self.current_loops,
            tool_failure_counts: self.tool_failure_counts.clone(),
            last_progress_fingerprint: self.last_progress_fingerprint.clone(),
            consecutive_progress_count: self.consecutive_progress_count,
            child_failure_count: self.child_failure_count,
        }
    }

    pub fn restore(state: LoopGuardState) -> Self {
        Self {
            max_loops_without_progress: state.max_loops_without_progress,
            max_tool_failures: state.max_tool_failures,
            max_consecutive_same_progress: state.max_consecutive_same_progress,
            max_child_failures: state.max_child_failures,
            current_loops: state.current_loops,
            tool_failure_counts: state.tool_failure_counts,
            last_progress_fingerprint: state.last_progress_fingerprint,
            consecutive_progress_count: state.consecutive_progress_count,
            child_failure_count: state.child_failure_count,
        }
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
    pub current_loops: u32,
    pub tool_failure_counts: std::collections::HashMap<String, u32>,
    pub last_progress_fingerprint: Option<(String, u64)>,
    pub consecutive_progress_count: u32,
    pub child_failure_count: u32,
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

    #[test]
    fn test_loop_guard_trips_on_tool_failure_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..4 {
            guard.register_failure("web_fetch", r#"{"url":"https://example.com/a"}"#, None);
            guard.register_progress("web_fetch", r#"{"url":"https://example.com/a"}"#);
            assert!(guard.check_loop().is_ok());
        }
        guard.register_failure("web_fetch", r#"{"url":"https://example.com/z"}"#, None);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_alternating_hosts_exhausts_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for i in 0..4 {
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

        for _ in 0..4 {
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

        for _ in 0..4 {
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
            guard.register_failure("web_fetch", r#"{"url":"https://denied.com"}"#, Some(&ToolErrorType::Permission));
            guard.register_progress("web_fetch", r#"{"url":"https://denied.com"}"#);
            assert!(guard.check_loop().is_ok());
        }

        guard.register_failure("web_fetch", r#"{"url":"https://denied.com"}"#, Some(&ToolErrorType::Permission));
        assert!(guard.check_loop().is_ok());
    }

    #[test]
    fn test_validation_errors_do_count_against_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..4 {
            guard.register_failure("web_fetch", r#"{"url":"https://bad.com"}"#, Some(&ToolErrorType::Validation));
            guard.register_progress("web_fetch", r#"{"url":"https://bad.com"}"#);
            assert!(guard.check_loop().is_ok());
        }
        guard.register_failure("web_fetch", r#"{"url":"https://bad.com"}"#, Some(&ToolErrorType::Validation));
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
}
