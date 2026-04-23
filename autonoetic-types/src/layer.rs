//! Layer — opaque compressed directory trees for dependency bundles.
//!
//! Layers are content-addressed, deduplicated directory trees stored by the gateway.
//! They are referenced by artifacts via layer IDs and mounted into sandboxes at
//! declared paths. The gateway does not interpret layer contents — it only stores,
//! verifies, and extracts them.

use serde::{Deserialize, Serialize};

/// Approval scope recorded at layer capture time.
///
/// Describes what network access was approved when this layer was built.
/// Used at mount time to verify the current session's approval scope covers
/// the layer's build-time approved hosts before mounting it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct LayerApprovalScope {
    /// Hosts that were detected and accessible when this layer was captured.
    /// These represent the operator-approved hosts for the build session —
    /// static analysis detects which hosts the build command accessed, and
    /// the presence of those hosts in the session's approval grants is what
    /// makes execution proceed. `None` scope means the layer was built without
    /// network access; `Some` with an empty list means network ran but no hosts
    /// were statically detected (conservative: treated as no scope restriction).
    pub approved_hosts: Vec<String>,
    /// Agent ID that built this layer.
    pub built_by_agent_id: String,
    /// ISO 8601 timestamp when the layer was captured.
    pub captured_at: String,
}

/// A layer manifest stored alongside the compressed archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerManifest {
    /// Short layer ID (e.g., "layer_a1b2c3d4")
    pub layer_id: String,
    /// Human-readable name given at capture time (e.g., "python-deps", "node_modules").
    /// Stored so approval messages can show a friendly name rather than a bare ID.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// SHA-256 digest of the compressed archive contents
    pub digest: String,
    /// Number of files in the layer
    pub file_count: usize,
    /// Uncompressed size in bytes
    pub size_bytes: u64,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Approval scope recorded at capture time.
    /// None for layers built before this feature was added (treated as no network scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_scope: Option<LayerApprovalScope>,
}

/// A reference to a layer within an artifact manifest.
///
/// Describes which layer to mount and where inside the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactLayer {
    /// Short layer ID (e.g., "layer_a1b2c3d4")
    pub layer_id: String,
    /// Human-readable name for the layer (e.g., "python-deps", "node_modules")
    pub name: String,
    /// Mount path inside the sandbox where this layer will be extracted
    pub mount_path: String,
    /// SHA-256 digest for integrity verification
    pub digest: String,
}

/// Metadata about a captured layer returned from sandbox.exec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedLayer {
    /// Short layer ID assigned after capture
    pub layer_id: String,
    /// Human-readable name
    pub name: String,
    /// Mount path that was captured
    pub mount_path: String,
    /// SHA-256 digest of the compressed archive
    pub digest: String,
    /// Number of files captured
    pub file_count: usize,
    /// Uncompressed size in bytes
    pub size_bytes: u64,
    /// Approval scope recorded at capture time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_scope: Option<LayerApprovalScope>,
}
