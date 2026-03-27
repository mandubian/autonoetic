//! Layer — opaque compressed directory trees for dependency bundles.
//!
//! Layers are content-addressed, deduplicated directory trees stored by the gateway.
//! They are referenced by artifacts via layer IDs and mounted into sandboxes at
//! declared paths. The gateway does not interpret layer contents — it only stores,
//! verifies, and extracts them.

use serde::{Deserialize, Serialize};

/// A layer manifest stored alongside the compressed archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerManifest {
    /// Short layer ID (e.g., "layer_a1b2c3d4")
    pub layer_id: String,
    /// SHA-256 digest of the compressed archive contents
    pub digest: String,
    /// Number of files in the layer
    pub file_count: usize,
    /// Uncompressed size in bytes
    pub size_bytes: u64,
    /// ISO 8601 creation timestamp
    pub created_at: String,
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
}
