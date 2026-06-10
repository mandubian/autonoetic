//! Runtime Lock types — the pinned execution closure for reproducible resolution.

use crate::layer::LayerApprovalScope;
use serde::{Deserialize, Serialize};

/// Derive the default env-var injection name from a service identifier.
/// Mirrors the gateway's `inject_as_for_service()` used by `skill_normalize`.
///
/// `"moltbook"` → `"MOLTBOOK_SECRET"`, `"my-api"` → `"MY_API_SECRET"`.
pub fn inject_as_for_service(service: &str) -> String {
    let mut s = String::new();
    for c in service.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_uppercase());
        } else {
            s.push('_');
        }
    }
    let s = s.trim_matches('_');
    if s.is_empty() {
        "SERVICE_SECRET".to_string()
    } else {
        format!("{s}_SECRET")
    }
}

/// A credential requirement declared in the runtime lock.
/// At spawn time the gateway resolves credentials by service name
/// and injects the secret as the env var derived by [`inject_as_for_service`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedCredentialMount {
    /// Service name matching the `credential.service` value in the store.
    pub service: String,
    /// Optional specific credential ID to inject at spawn time.
    /// When absent, resolve by service name (first match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
}

/// A pinned artifact reference inside the runtime lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedArtifact {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub source: String,
}

/// Gateway binary reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedGateway {
    pub artifact: String,
    pub version: String,
    /// Source fingerprint (version + git state), set at compile time.
    pub sha256: String,
    /// SHA-256 of the currently running gateway executable bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<String>,
    /// Human-readable source tag (e.g. "0.1.0+a1b2c3d4e5f6.dirty").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_tag: Option<String>,
    pub signature: Option<String>,
}

/// SDK version reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedSdk {
    pub version: String,
}

/// Sandbox backend reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedSandbox {
    pub backend: String,
}

/// Pinned dependencies for sandbox execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedDependencySet {
    pub runtime: String,
    pub packages: Vec<String>,
}

/// A pinned layer mount inside the runtime closure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedLayerMount {
    /// Layer ID.
    pub layer_id: String,
    /// Layer digest for integrity verification.
    pub digest: String,
    /// Mount path inside the sandbox.
    pub mount_path: String,
    /// Approval scope recorded at layer capture time.
    /// None for layers built before this feature was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_scope: Option<LayerApprovalScope>,
}

/// The complete `runtime.lock` file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLock {
    pub gateway: LockedGateway,
    pub sdk: LockedSdk,
    pub sandbox: LockedSandbox,
    #[serde(default)]
    pub dependencies: Vec<LockedDependencySet>,
    #[serde(default)]
    pub artifacts: Vec<LockedArtifact>,
    /// Pinned layer mounts forming the execution closure.
    #[serde(default)]
    pub layers: Vec<LockedLayerMount>,
    /// Credential services this agent requires at spawn time.
    /// The env-var name is derived deterministically via [`inject_as_for_service`].
    #[serde(default)]
    pub credentials: Vec<LockedCredentialMount>,
}

impl RuntimeLock {
    /// True when the closure declares runtime-installed (e.g. `pip install`)
    /// dependencies. These are resolved *at spawn time* and need network — the
    /// opposite of an embedded, content-addressed [`LockedLayerMount`].
    pub fn has_runtime_pip_dependencies(&self) -> bool {
        !self.dependencies.is_empty()
    }

    /// True when the closure is **dependency-locked**: every dependency is baked
    /// into a pinned, content-addressed `layers` mount (or there are none), with
    /// no runtime-install step. Locked closures are reproducible and importable
    /// offline — a precondition for `CapsuleMode::Hermetic`/`Replay` export.
    pub fn is_dependency_locked(&self) -> bool {
        !self.has_runtime_pip_dependencies()
    }
}

#[cfg(test)]
mod lock_state_tests {
    use super::*;

    fn empty_lock() -> RuntimeLock {
        RuntimeLock {
            gateway: LockedGateway {
                artifact: String::new(),
                version: String::new(),
                sha256: String::new(),
                binary_sha256: None,
                build_tag: None,
                signature: None,
            },
            sdk: LockedSdk {
                version: String::new(),
            },
            sandbox: LockedSandbox {
                backend: String::new(),
            },
            dependencies: vec![],
            artifacts: vec![],
            layers: vec![],
            credentials: vec![],
        }
    }

    #[test]
    fn no_deps_is_locked() {
        let lock = empty_lock();
        assert!(lock.is_dependency_locked());
        assert!(!lock.has_runtime_pip_dependencies());
    }

    #[test]
    fn runtime_pip_deps_are_not_locked() {
        let mut lock = empty_lock();
        lock.dependencies.push(LockedDependencySet {
            runtime: "python".to_string(),
            packages: vec!["requests".to_string()],
        });
        assert!(!lock.is_dependency_locked());
        assert!(lock.has_runtime_pip_dependencies());
    }
}
