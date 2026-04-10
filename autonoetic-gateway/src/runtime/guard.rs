//! Loop Guard Mechanism.
//!
//! Prevents agents from getting stuck in infinite reasoning loops
//! without making progress.

fn extract_failure_key(tool_name: &str, arguments: &str) -> String {
    if let Some(host) = extract_url_host_from_json(arguments) {
        return format!("{}:{}", tool_name, host);
    }
    if let Some(path) = extract_path_from_json(arguments) {
        return format!("{}:{}", tool_name, path);
    }
    if let Some(query) = extract_query_from_json(arguments) {
        return format!("{}:{}", tool_name, query);
    }
    format!("{}:{}", tool_name, arguments)
}

fn extract_url_host_from_json(arguments: &str) -> Option<String> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return None;
    };
    let url = val.get("url").and_then(|v| v.as_str())?;
    extract_host_from_url(url)
}

fn extract_path_from_json(arguments: &str) -> Option<String> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return None;
    };
    val.get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_query_from_json(arguments: &str) -> Option<String> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return None;
    };
    val.get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn extract_host_from_url(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?i)^[a-z]+://([^/:]+)").ok()?;
    let captures = re.captures(url)?;
    let host = captures.get(1)?.as_str();
    if host.is_empty() {
        None
    } else {
        Some(host.trim_end_matches('.').to_ascii_lowercase())
    }
}

pub struct LoopGuard {
    max_loops_without_progress: u32,
    current_loops: u32,
    last_failure_hash: Option<u64>,
    consecutive_failures: u32,
}

impl LoopGuard {
    pub fn new(max_loops_without_progress: u32) -> Self {
        Self {
            max_loops_without_progress,
            current_loops: 0,
            last_failure_hash: None,
            consecutive_failures: 0,
        }
    }

    /// Call this before each agent reasoning cycle.
    pub fn check_loop(&mut self) -> anyhow::Result<()> {
        if self.current_loops >= self.max_loops_without_progress {
            anyhow::bail!(
                "LoopGuard tripped: Agent executed {} cycles without meaningful progress.",
                self.current_loops
            );
        }

        if self.consecutive_failures >= 3 {
            anyhow::bail!(
                "LoopGuard tripped: Agent is repeating a failing action (3 consecutive identical failures). Breaking loop to prevent resource waste."
            );
        }

        self.current_loops += 1;
        Ok(())
    }

    /// Track a tool failure to detect redundant failing loops.
    pub fn register_failure(&mut self, tool_name: &str, arguments: &str) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let failure_key = extract_failure_key(tool_name, arguments);
        let mut hasher = DefaultHasher::new();
        failure_key.hash(&mut hasher);
        let current_hash = hasher.finish();

        if let Some(last_hash) = self.last_failure_hash {
            if last_hash == current_hash {
                self.consecutive_failures += 1;
            } else {
                self.consecutive_failures = 1;
                self.last_failure_hash = Some(current_hash);
            }
        } else {
            self.consecutive_failures = 1;
            self.last_failure_hash = Some(current_hash);
        }
    }

    /// Call this when the agent takes a meaningful external action (e.g., writes a file, calls a tool successfully).
    pub fn register_progress(&mut self) {
        self.current_loops = 0;
    }

    /// Capture the current guard state for serialization (e.g. turn continuation checkpoint).
    pub fn snapshot(&self) -> LoopGuardState {
        LoopGuardState {
            max_loops_without_progress: self.max_loops_without_progress,
            current_loops: self.current_loops,
            last_failure_hash: self.last_failure_hash,
            consecutive_failures: self.consecutive_failures,
        }
    }

    /// Restore guard state from a previously captured snapshot.
    pub fn restore(state: LoopGuardState) -> Self {
        Self {
            max_loops_without_progress: state.max_loops_without_progress,
            current_loops: state.current_loops,
            last_failure_hash: state.last_failure_hash,
            consecutive_failures: state.consecutive_failures,
        }
    }
}

/// Serializable snapshot of a [`LoopGuard`] for turn continuation checkpoints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LoopGuardState {
    pub max_loops_without_progress: u32,
    pub current_loops: u32,
    pub last_failure_hash: Option<u64>,
    pub consecutive_failures: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_guard_trips() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok()); // 1
        assert!(guard.check_loop().is_ok()); // 2
        assert!(guard.check_loop().is_ok()); // 3
        assert!(guard.check_loop().is_err()); // Trips on 4th check
    }

    #[test]
    fn test_loop_guard_redundant_failures() {
        let mut guard = LoopGuard::new(10);
        assert!(guard.check_loop().is_ok());

        // Simulate 3 consecutive identical failures
        guard.register_failure("test_tool", "{\"a\": 1}");
        assert!(guard.check_loop().is_ok());

        guard.register_failure("test_tool", "{\"a\": 1}");
        assert!(guard.check_loop().is_ok());

        guard.register_failure("test_tool", "{\"a\": 1}");
        assert!(guard.check_loop().is_err()); // Should trip on 4th check after 3 failures
    }

    #[test]
    fn test_loop_guard_failure_persists_across_progress() {
        let mut guard = LoopGuard::new(10);

        guard.register_failure("test_tool", "{\"a\": 1}");
        guard.register_failure("test_tool", "{\"a\": 1}");
        guard.register_progress();

        guard.register_failure("test_tool", "{\"a\": 1}");
        assert!(guard.check_loop().is_err());
    }

    #[test]
    fn test_loop_guard_progress_resets_loops_not_failures() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        assert!(guard.check_loop().is_ok());
        guard.register_progress();
        assert!(guard.check_loop().is_ok());
    }

    #[test]
    fn test_loop_guard_different_hosts_different_keys() {
        let mut guard = LoopGuard::new(3);
        assert!(guard.check_loop().is_ok());

        guard.register_failure("web.fetch", r#"{"url":"https://example.com/a"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://other.com/b"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://example.com/b"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://example.com/c"}"#);
        assert!(guard.check_loop().is_ok());
        guard.register_progress();

        guard.register_failure("web.fetch", r#"{"url":"https://example.com/d"}"#);
        assert!(guard.check_loop().is_err());
    }
}
