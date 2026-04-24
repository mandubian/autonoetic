//! Tier 2 Memory Object — Gateway-substrate persistent memory with provenance tracking.

use serde::{Deserialize, Deserializer, Serialize};

/// Visibility scope for a memory entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryVisibility {
    /// Only the owning / writing agent can read.
    Private,
    /// Any agent participating in the same root `session_id` can read.
    Session { session_id: String },
    /// Any agent in any session can read.
    Global,
}

impl Default for MemoryVisibility {
    fn default() -> Self {
        MemoryVisibility::Private
    }
}

impl<'de> Deserialize<'de> for MemoryVisibility {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        decode_visibility_json(v).map_err(serde::de::Error::custom)
    }
}

fn decode_visibility_json(v: serde_json::Value) -> Result<MemoryVisibility, String> {
    match v {
        serde_json::Value::String(s) => match s.as_str() {
            "private" => Ok(MemoryVisibility::Private),
            "global" => Ok(MemoryVisibility::Global),
            // Legacy Tier-2 "shared" — treat as global (no allowed_agents list anymore).
            "shared" => Ok(MemoryVisibility::Global),
            // Unknown legacy values: safest default is private.
            _ => Ok(MemoryVisibility::Private),
        },
        serde_json::Value::Object(_) => {
            #[derive(Deserialize)]
            #[serde(tag = "kind", rename_all = "snake_case")]
            enum Tagged {
                Private,
                Session { session_id: String },
                Global,
            }
            match serde_json::from_value::<Tagged>(v) {
                Ok(Tagged::Private) => Ok(MemoryVisibility::Private),
                Ok(Tagged::Session { session_id }) => Ok(MemoryVisibility::Session { session_id }),
                Ok(Tagged::Global) => Ok(MemoryVisibility::Global),
                Err(_) => Ok(MemoryVisibility::Private),
            }
        }
        _ => Ok(MemoryVisibility::Private),
    }
}

/// Source type for a memory record (tracks origin of the fact).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceType {
    #[default]
    AgentWrite,
    ToolOutput,
    IngestedEvent,
    ScheduledAction,
    Manual,
    /// Extracted by the post-session digest LLM from a completed session.
    SessionDigest,
}

/// Lineage entry tracks the ancestry of a memory record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLineageEntry {
    pub source_memory_id: String,
    pub operation: String,
    pub agent_id: String,
    pub timestamp: String,
}

/// A single Tier 2 memory object stored in the Gateway substrate with full provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryObject {
    /// Unique identifier for this memory record.
    pub memory_id: String,

    /// Scope/namespace for organizing memory (e.g., "facts", "preferences", "context").
    pub scope: String,

    /// Agent that owns this memory record (typically the agent that created it).
    pub owner_agent_id: String,

    /// Agent that wrote/updated this record (for tracking cross-agent sharing).
    pub writer_agent_id: String,

    /// Type of source that created this record.
    #[serde(default)]
    pub source_type: MemorySourceType,

    /// Reference to the causal chain entry, session ID, or other origin artifact.
    /// Format: "session:<session_id>:turn:<turn_id>" or "causal:<log_id>".
    pub source_ref: String,

    /// ISO 8601 timestamp when the record was created.
    pub created_at: String,

    /// ISO 8601 timestamp when the record was last updated.
    pub updated_at: String,

    /// The actual content/value of the memory.
    pub content: String,

    /// SHA-256 hash of the content for integrity verification.
    pub content_hash: String,

    /// Optional confidence score (0.0-1.0) for the fact's reliability.
    #[serde(default)]
    pub confidence: Option<f64>,

    /// Optional tags for categorization and filtering.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Optional lineage tracking for derived/transformed memories.
    #[serde(default)]
    pub lineage: Vec<MemoryLineageEntry>,

    /// Visibility/ACL for controlling access and sharing.
    #[serde(default)]
    pub visibility: MemoryVisibility,

    /// When this memory stops being readable (`None` = never expires). RFC 3339 UTC from the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// Revision ID of the agent that wrote this memory (from session_agent_bindings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,

    /// Session ID from the binding that was active when this memory was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_session_id: Option<String>,

    /// Alias reference (alias_id) from the binding when this memory was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias_ref: Option<String>,

    /// When non-None, this memory has been quarantined (e.g. by revision rollback).
    /// The value is a human-readable reason string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
}

impl MemoryObject {
    /// Creates a new MemoryObject with required fields.
    pub fn new(
        memory_id: String,
        scope: String,
        owner_agent_id: String,
        writer_agent_id: String,
        source_ref: String,
        content: String,
    ) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_hash = hex::encode(hasher.finalize());

        let now = chrono::Utc::now().to_rfc3339();

        Self {
            memory_id,
            scope,
            owner_agent_id,
            writer_agent_id,
            source_type: MemorySourceType::default(),
            source_ref,
            created_at: now.clone(),
            updated_at: now,
            content,
            content_hash,
            confidence: None,
            tags: Vec::new(),
            lineage: Vec::new(),
            visibility: MemoryVisibility::default(),
            expires_at: None,
            revision_id: None,
            binding_session_id: None,
            alias_ref: None,
            quarantine_reason: None,
        }
    }

    /// Whether this memory is past its expiry (compared to `now` as RFC 3339).
    pub fn is_expired_at(&self, now_rfc3339: &str) -> bool {
        let Some(exp) = self.expires_at.as_deref() else {
            return false;
        };
        exp <= now_rfc3339
    }

    /// Updates the content and returns a new MemoryObject with updated timestamps and hash.
    pub fn update_content(mut self, new_content: String, writer_agent_id: String) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(new_content.as_bytes());

        self.content = new_content;
        self.content_hash = hex::encode(hasher.finalize());
        self.writer_agent_id = writer_agent_id;
        self.updated_at = chrono::Utc::now().to_rfc3339();

        self
    }

    /// Makes this memory globally visible.
    pub fn make_global(mut self) -> Self {
        self.visibility = MemoryVisibility::Global;
        self.updated_at = chrono::Utc::now().to_rfc3339();

        self
    }

    /// Checks if an agent is allowed to read this memory.
    ///
    /// For [`MemoryVisibility::Session`], `reader_session_id` must match the memory's
    /// `session_id` unless the agent is the reader or writer.
    pub fn is_readable_by(&self, agent_id: &str, reader_session_id: Option<&str>) -> bool {
        match &self.visibility {
            MemoryVisibility::Private => {
                self.owner_agent_id == agent_id || self.writer_agent_id == agent_id
            }
            MemoryVisibility::Session { session_id } => {
                self.owner_agent_id == agent_id
                    || self.writer_agent_id == agent_id
                    || reader_session_id
                        .map(|s| s == session_id.as_str())
                        .unwrap_or(false)
            }
            MemoryVisibility::Global => true,
        }
    }

    /// Checks if an agent is allowed to write/update this memory.
    pub fn is_writable_by(&self, agent_id: &str) -> bool {
        self.owner_agent_id == agent_id || self.writer_agent_id == agent_id
    }
}
