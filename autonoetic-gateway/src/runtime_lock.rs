//! Runtime Lock resolution and drift detection (R+7 / R+18).

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
            return DriftCheckResult::Skipped(DriftSkippedReason::LockMalformed(
                e.to_string(),
            ));
        }
    };

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
