//! Runtime Lock types — the pinned execution closure for reproducible resolution.

use serde::{Deserialize, Serialize};

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
}
