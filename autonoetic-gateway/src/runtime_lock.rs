//! Runtime Lock resolution and drift detection (P-8.12).

use autonoetic_types::runtime_lock::RuntimeLock;
use std::path::Path;

/// Parse and resolve a `runtime.lock` YAML file.
pub fn resolve_runtime_lock(path: &Path) -> anyhow::Result<RuntimeLock> {
    let contents = std::fs::read_to_string(path)?;
    let lock: RuntimeLock = serde_yaml::from_str(&contents)?;
    Ok(lock)
}

/// Detected drift between a recorded runtime.lock and the current gateway.
#[derive(Debug, Clone)]
pub struct RuntimeLockDrift {
    pub locked_build_sha256: String,
    pub current_build_sha256: &'static str,
    pub locked_binary_sha256: Option<String>,
    pub current_binary_sha256: Option<String>,
}

/// Reason drift enforcement was skipped (used for causal-event logging).
#[derive(Debug, Clone)]
pub enum DriftSkippedReason {
    LockAbsent,
    LockMalformed(String),
    /// The lock parses but its `gateway.sha256` is empty — it has not been
    /// pinned to a build yet (e.g. a freshly generated wrapper whose lock is
    /// "computed on first gateway load"). There is nothing to compare against,
    /// so drift is not enforced.
    LockUnpinned,
}

/// Result of the drift check: either clean, drift detected, or enforcement skipped.
#[derive(Debug)]
pub enum DriftCheckResult {
    Clean,
    Drift(RuntimeLockDrift),
    Skipped(DriftSkippedReason),
}

/// Compare the runtime.lock in `agent_dir` against the running gateway.
///
/// - **Absent lock** → `Skipped(LockAbsent)` — not all agents carry a runtime.lock
///   (legacy or manually-created test agents).
/// - **Malformed lock** → `Skipped(LockMalformed(..))` — logged but not fatal.
/// - **Build SHA mismatch** → `Drift(..)`.
/// - **Binary SHA mismatch** → `Drift(..)`.
/// - **Locked binary SHA present but current binary unreadable** → `Drift(..)`
///   (cannot verify, treated as drift).
/// - **All match** → `Clean`.
pub fn check_runtime_lock_drift(agent_dir: &Path) -> DriftCheckResult {
    let lock_path = agent_dir.join("runtime.lock");
    if !lock_path.exists() {
        return DriftCheckResult::Skipped(DriftSkippedReason::LockAbsent);
    }

    let lock = match resolve_runtime_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            return DriftCheckResult::Skipped(DriftSkippedReason::LockMalformed(e.to_string()));
        }
    };

    // An empty locked build SHA means the lock was generated but never pinned
    // (the generator writes `sha256: ""` with "computed on first gateway load").
    // There is no recorded build to compare against, so treat it as unpinned
    // rather than reporting spurious drift against the current build.
    if lock.gateway.sha256.trim().is_empty() {
        return DriftCheckResult::Skipped(DriftSkippedReason::LockUnpinned);
    }

    let current_build_sha = crate::runtime::install_contract::GATEWAY_BUILD_SHA256;

    if lock.gateway.sha256 != current_build_sha {
        return DriftCheckResult::Drift(RuntimeLockDrift {
            locked_build_sha256: lock.gateway.sha256,
            current_build_sha256: current_build_sha,
            locked_binary_sha256: lock.gateway.binary_sha256.clone(),
            current_binary_sha256: None,
        });
    }

    if let Some(ref locked_bin) = lock.gateway.binary_sha256 {
        match crate::runtime::install_contract::running_binary_sha256() {
            Ok(current_bin) if locked_bin != &current_bin => {
                return DriftCheckResult::Drift(RuntimeLockDrift {
                    locked_build_sha256: lock.gateway.sha256,
                    current_build_sha256: current_build_sha,
                    locked_binary_sha256: Some(locked_bin.clone()),
                    current_binary_sha256: Some(current_bin),
                });
            }
            Err(_) => {
                return DriftCheckResult::Drift(RuntimeLockDrift {
                    locked_build_sha256: lock.gateway.sha256,
                    current_build_sha256: current_build_sha,
                    locked_binary_sha256: Some(locked_bin.clone()),
                    current_binary_sha256: None,
                });
            }
            _ => {}
        }
    }

    DriftCheckResult::Clean
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the runtime.lock that generate_wrapper.py emits for a freshly
    // generated wrapper: a parseable lock whose build sha256 is intentionally
    // empty ("computed on first gateway load"). It must be treated as unpinned,
    // not as drift against the current build (regression: wrapper execution
    // failed with "runtime lock drift detected (build_sha256): locked=").
    const UNPINNED_LOCK: &str = "# Generated runtime.lock - sha256 is computed on first gateway load.\n\
gateway:\n\
\x20 artifact: \"marketplace://gateway/autonoetic-gateway\"\n\
\x20 version: \"0.1.0\"\n\
\x20 sha256: \"\"\n\
sdk:\n\
\x20 version: \"0.1.0\"\n\
sandbox:\n\
\x20 backend: \"bubblewrap\"\n\
dependencies: []\n\
artifacts: []\n";

    #[test]
    fn unpinned_lock_is_skipped_not_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("runtime.lock"), UNPINNED_LOCK).expect("write lock");
        match check_runtime_lock_drift(dir.path()) {
            DriftCheckResult::Skipped(DriftSkippedReason::LockUnpinned) => {}
            other => panic!("expected Skipped(LockUnpinned), got {other:?}"),
        }
    }

    #[test]
    fn absent_lock_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        match check_runtime_lock_drift(dir.path()) {
            DriftCheckResult::Skipped(DriftSkippedReason::LockAbsent) => {}
            other => panic!("expected Skipped(LockAbsent), got {other:?}"),
        }
    }
}
