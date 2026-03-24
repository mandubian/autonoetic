//! Artifact Handle — content-addressed immutable data objects.

use serde::{Deserialize, Serialize};

/// The kind of artifact stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Binary,
    SkillBundle,
    Dataset,
    GatewayRuntime,
    Report,
}

/// Provenance of the artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRuntime {
    pub gateway_version: String,
    pub skill_name: Option<String>,
}

/// Visibility scope.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Private,
    Shared,
    Capsule,
}

/// An immutable content-addressed artifact handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactHandle {
    pub artifact_id: String,
    pub sha256: String,
    pub kind: ArtifactKind,
    pub owner_id: String,
    #[serde(default)]
    pub visibility: Visibility,
    pub size_bytes: u64,
    pub mime_type: Option<String>,
    pub created_at: String,
    pub summary: Option<String>,
    pub source_runtime: Option<SourceRuntime>,
}

// ---------------------------------------------------------------------------
// Artifact Bundle — closed file closure for review/install/execution
// ---------------------------------------------------------------------------

/// A single file entry in an artifact bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactFileEntry {
    /// Filename within the artifact (e.g., "src/main.py")
    pub name: String,
    /// Content handle in the content store (sha256:...)
    pub handle: String,
    /// Short alias for LLM-friendly reference
    pub alias: String,
}

/// An immutable artifact bundle — a closed set of files for review/install/execution.
///
/// Artifacts are the only units that may cross trust boundaries.
/// They are built from session content and are immutable once created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactBundle {
    /// Short unique ID (e.g., "art_a1b2c3d4")
    pub artifact_id: String,
    /// Files included in the artifact
    pub files: Vec<ArtifactFileEntry>,
    /// Optional entrypoints (e.g., ["src/main.py"])
    #[serde(default)]
    pub entrypoints: Vec<String>,
    /// SHA-256 digest of the full manifest (content-addressable identity)
    pub digest: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// Session that built this artifact
    pub builder_session_id: String,
    /// True if this artifact was reused from an existing artifact with same inputs
    #[serde(default)]
    pub reused: bool,
}

// ---------------------------------------------------------------------------
// Artifact Reference Records (short scoped refs -> canonical artifact identity)
// ---------------------------------------------------------------------------

/// Scope namespace for short artifact references.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRefScopeType {
    Session,
    Workflow,
    Global,
}

impl ArtifactRefScopeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Workflow => "workflow",
            Self::Global => "global",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            "workflow" => Some(Self::Workflow),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

/// Durable mapping from a short, LLM-friendly ref_id to a canonical artifact identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRefRecord {
    /// Short alias used by agents in prompts/tool args (example: "ar.wf9f3.004.k7p2").
    pub ref_id: String,
    /// Scope namespace this ref belongs to.
    pub scope_type: ArtifactRefScopeType,
    /// Session ID, workflow ID, or "__global__" depending on scope_type.
    pub scope_id: String,
    /// Artifact ID (art_*).
    pub artifact_id: String,
    /// Canonical SHA-256 digest of artifact manifest for integrity verification.
    pub artifact_digest: String,
    /// Agent that created this short reference.
    pub created_by_agent_id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Optional RFC3339 expiry timestamp.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Optional RFC3339 revocation timestamp.
    #[serde(default)]
    pub revoked_at: Option<String>,
}
