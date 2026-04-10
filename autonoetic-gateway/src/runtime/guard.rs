//! Loop Guard Mechanism.
//!
//! Prevents agents from getting stuck in infinite reasoning loops
//! without making progress.
//!
//! Two independent trip conditions:
//! 1. **Max loops without progress**: Agent executed N cycles without any
//!    successful tool call resetting the counter.
//! 2. **Tool failure budget exhausted**: A single tool has failed more than
//!    `max_tool_failures` times total in the session, regardless of arguments
//!    or targets. This catches alternating-failure patterns where the agent
//!    tries different hosts/args but the same tool keeps failing.
//!
//! The per-tool budget is tool-name-scoped (not argument-scoped), making it
//! generic for all agent types. URL/host extraction is used only for the
//! (now secondary) consecutive-identical-failure fast path.

use std::collections::HashMap;

const MAX_TOOL_FAILURES: u32 = 5;

pub struct LoopGuard {
    max_loops_without_progress: u32,
    current_loops: u32,
    tool_failure_counts: HashMap<String, u32>,
}

impl LoopGuard {
    pub fn new(max_loops_without_progress: u32) -> Self {
        Self {
            max_loops_without_progress,
            current_loops: 0,
            tool_failure_counts: HashMap::new(),
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
            if *count >= MAX_TOOL_FAILURES {
                anyhow::bail!(
                    "LoopGuard tripped: Tool '{}' has failed {} times in this session. \
                     Breaking loop to prevent resource waste.",
                    tool_name,
                    count
                );
            }
        }

        self.current_loops += 1;
        Ok(())
    }

    pub fn register_failure(&mut self, tool_name: &str, _arguments: &str) {
        *self
            .tool_failure_counts
            .entry(tool_name.to_string())
            .or_insert(0) += 1;
    }

    pub fn register_progress(&mut self) {
        self.current_loops = 0;
    }

    pub fn snapshot(&self) -> LoopGuardState {
        LoopGuardState {
            max_loops_without_progress: self.max_loops_without_progress,
            current_loops: self.current_loops,
            tool_failure_counts: self.tool_failure_counts.clone(),
        }
    }

    pub fn restore(state: LoopGuardState) -> Self {
        Self {
            max_loops_without_progress: state.max_loops_without_progress,
            current_loops: state.current_loops,
            tool_failure_counts: state.tool_failure_counts,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoopGuardState {
    pub max_loops_without_progress: u32,
    pub current_loops: u32,
    pub tool_failure_counts: HashMap<String, u32>,
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
        guard.register_progress();
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
            guard.register_failure("web.fetch", r#"{"url":"https://example.com/a"}"#);
            assert!(guard.check_loop().is_ok());
            guard.register_progress();
        }
        guard.register_failure("web.fetch", r#"{"url":"https://example.com/z"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_alternating_hosts_exhausts_budget() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        guard.register_failure("web.fetch", r#"{"url":"https://accuweather.com/a"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://weather.com/b"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://accuweather.com/c"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://weather.com/d"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://accuweather.com/e"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_different_tools_tracked_separately() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..4 {
            guard.register_failure("web.fetch", r#"{"url":"https://example.com"}"#);
            guard.register_failure("sandbox.exec", r#"{"command":"python3 test.py"}"#);
            guard.register_progress();
            assert!(guard.check_loop().is_ok());
        }

        guard.register_failure("web.fetch", r#"{"url":"https://example.com"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_non_url_tools_count_failures() {
        let mut guard = LoopGuard::new(100);
        assert!(guard.check_loop().is_ok());

        for _ in 0..4 {
            guard.register_failure("sandbox.exec", r#"{"command":"python3 test.py"}"#);
            assert!(guard.check_loop().is_ok());
            guard.register_progress();
        }
        guard.register_failure("sandbox.exec", r#"{"command":"python3 other.py"}"#);
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_snapshot_restore() {
        let mut guard = LoopGuard::new(3);
        guard.register_failure("web.fetch", r#"{"url":"https://example.com"}"#);
        guard.register_failure("web.fetch", r#"{"url":"https://other.com"}"#);
        assert_eq!(guard.check_loop().unwrap(), ());

        let snap = guard.snapshot();
        let restored = LoopGuard::restore(snap);

        assert_eq!(restored.current_loops, 1);
        assert_eq!(*restored.tool_failure_counts.get("web.fetch").unwrap(), 2);
    }
}
