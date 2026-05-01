//! Runtime Lock resolution and drift detection.

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

/// Compare the runtime.lock in `agent_dir` against the running gateway.
///
/// Returns `Ok(())` if the lock is absent or matches, `Err(RuntimeLockDrift)` on mismatch.
pub fn check_runtime_lock_drift(agent_dir: &Path) -> Result<(), RuntimeLockDrift> {
    let lock_path = agent_dir.join("runtime.lock");
    if !lock_path.exists() {
        return Ok(());
    }

    let lock = match resolve_runtime_lock(&lock_path) {
        Ok(l) => l,
        Err(_) => return Ok(()),
    };

    let current_build_sha = env!("GATEWAY_BUILD_SHA256");

    if lock.gateway.sha256 != current_build_sha {
        return Err(RuntimeLockDrift {
            locked_build_sha256: lock.gateway.sha256,
            current_build_sha256: current_build_sha,
            locked_binary_sha256: lock.gateway.binary_sha256.clone(),
            current_binary_sha256: None,
        });
    }

    if let Some(ref locked_bin) = lock.gateway.binary_sha256 {
        if let Ok(current_bin) = crate::runtime::install_contract::running_binary_sha256() {
            if locked_bin != &current_bin {
                return Err(RuntimeLockDrift {
                    locked_build_sha256: lock.gateway.sha256,
                    current_build_sha256: current_build_sha,
                    locked_binary_sha256: Some(locked_bin.clone()),
                    current_binary_sha256: Some(current_bin),
                });
            }
        }
    }

    Ok(())
}
