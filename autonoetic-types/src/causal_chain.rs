//! Causal Chain log entry — immutable hash-chain audit trail.

use serde::{Deserialize, Serialize};

/// Status of a causal chain entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryStatus {
    Success,
    Denied,
    Error,
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "SUCCESS"),
            Self::Denied => write!(f, "DENIED"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// A single entry in the append-only `.jsonl` Causal Chain log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalChainEntry {
    pub timestamp: String,
    pub log_id: String,
    pub actor_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub event_seq: u64,
    pub category: String,
    pub action: String,
    pub target: Option<String>,
    pub status: EntryStatus,
    pub reason: Option<String>,
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub payload_hash: Option<String>,
    pub prev_hash: String,
    #[serde(default)]
    pub entry_hash: String,
}

/// Causal event record for storage in gateway.db causal_events table.
/// Matches the schema for queryable event storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEventRecord {
    pub event_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub event_seq: u64,
    pub timestamp: String,
    pub category: String,
    pub action: String,
    pub status: String,
    pub target: Option<String>,
    pub payload: Option<String>,
    pub payload_ref: Option<String>,
    pub evidence_ref: Option<String>,
    pub reason: Option<String>,
}
