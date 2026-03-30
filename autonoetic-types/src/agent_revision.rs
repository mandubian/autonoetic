//! Agent revision types — immutable, content-addressed agent snapshots.
//!
//! An agent revision is the execution unit for agent sessions. It captures
//! an immutable snapshot of SKILL.md, agent files, capabilities, runtime
//! metadata, runtime.lock, and any referenced skills/artifacts/layers.
//!
//! Sessions pin a concrete revision at start time; later alias promotion
//! does not affect already-running sessions.

use serde::{Deserialize, Serialize};

/// A fully qualified immutable agent reference.
///
/// Format: `<agent_id>@rev_sha256:<64 hex chars>`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRef {
    pub agent_id: String,
    pub revision_id: String,
}

impl AgentRef {
    pub fn new(agent_id: String, revision_id: String) -> Self {
        Self {
            agent_id,
            revision_id,
        }
    }

    /// Parse an agent_ref string in the format `agent_id@revision_id`.
    ///
    /// Validation rules:
    /// - Exactly one `@` separator
    /// - agent_id must match `[a-z0-9][a-z0-9._-]*` (no `@`)
    /// - revision_id must start with `rev_sha256:` followed by exactly 64 lowercase hex chars
    pub fn parse(s: &str) -> Option<Self> {
        let at_pos = s.find('@')?;
        if at_pos == 0 || at_pos != s.rfind('@').unwrap_or(0) {
            return None;
        }
        let agent_id = &s[..at_pos];
        let revision_id = &s[at_pos + 1..];

        if agent_id.is_empty() || revision_id.is_empty() {
            return None;
        }

        if !Self::is_valid_agent_id(agent_id) {
            return None;
        }

        if !Self::is_valid_revision_id(revision_id) {
            return None;
        }

        Some(Self {
            agent_id: agent_id.to_string(),
            revision_id: revision_id.to_string(),
        })
    }

    fn is_valid_agent_id(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let first = s.chars().next().unwrap();
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return false;
        }
        s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
        })
    }

    fn is_valid_revision_id(s: &str) -> bool {
        let prefix = "rev_sha256:";
        if !s.starts_with(prefix) {
            return false;
        }
        let hex = &s[prefix.len()..];
        if hex.len() != 64 {
            return false;
        }
        hex.chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_lowercase() || c.is_ascii_digit()))
    }

    /// Format as `agent_id@revision_id`.
    pub fn to_string(&self) -> String {
        format!("{}@{}", self.agent_id, self.revision_id)
    }
}

/// Lifecycle status of an agent revision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRevisionStatus {
    /// Candidate revision not yet promoted to any alias.
    Candidate,
    /// Revision is active (pointed to by at least one alias).
    Ready,
    /// Revision has been superseded and is no longer active.
    Archived,
    /// Revision was rejected (e.g., failed evaluation).
    Rejected,
}

/// Durable record of an immutable agent revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRevisionRecord {
    /// Content-addressed revision ID (e.g., `rev_sha256:abcd1234...`).
    pub revision_id: String,
    /// Logical agent name this revision belongs to.
    pub agent_id: String,
    /// Base revision this was derived from (lineage).
    pub base_revision_id: Option<String>,
    /// Source artifact ID when created from an artifact bundle.
    pub artifact_id: Option<String>,
    /// Content digest (same digest family as revision_id).
    pub content_digest: String,
    /// Hash of the pinned runtime.lock (reproducibility binding).
    pub runtime_lock_hash: String,
    /// Hash of SKILL.md for quick integrity checks.
    pub manifest_hash: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Actor kind that created this revision (`agent`, `user`, `system`, `peer`).
    pub created_by_type: String,
    /// Actor ID that created this revision.
    pub created_by_id: String,
    /// Source kind: `artifact`, `capsule_import`, `peer_import`.
    pub source_kind: String,
    /// Source reference (artifact id, capsule id, or peer ref).
    pub source_ref: Option<String>,
    /// Origin node for federation provenance.
    pub origin_node_id: String,
    /// Trust domain: `local`, `partner`, `foreign`, `untrusted`.
    pub trust_domain: String,
    /// Current lifecycle status.
    pub status: AgentRevisionStatus,
    /// Arbitrary metadata (JSON).
    pub metadata_json: serde_json::Value,
}

/// Mutable alias binding from a stable name to a revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAliasRecord {
    /// Alias ID (MVP: equals `agent_id`).
    pub alias_id: String,
    /// Logical agent owner.
    pub agent_id: String,
    /// Target revision ID.
    pub revision_id: String,
    /// RFC3339 update timestamp.
    pub updated_at: String,
    /// Actor kind that updated this alias.
    pub updated_by_type: String,
    /// Actor ID that updated this alias.
    pub updated_by_id: String,
    /// Optional free-text reason for the update.
    pub reason: Option<String>,
}

/// Session-to-agent-revision binding, pinned at session start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAgentBinding {
    /// Exact session ID.
    pub session_id: String,
    /// Root session ID for grouping related sessions.
    pub root_session_id: String,
    /// Resolved alias ID (None if resolved directly from agent_ref without alias).
    pub alias_id: Option<String>,
    /// Logical agent ID.
    pub agent_id: String,
    /// Pinned immutable revision ID.
    pub revision_id: String,
    /// Pinned runtime closure hash.
    pub runtime_lock_hash: String,
    /// Home node for future distributed placement.
    pub home_node_id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// The original target the caller requested (agent_id or agent_ref string).
    pub requested_target: String,
}

/// Type of promotion action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromotionKind {
    /// Forward promotion to a newer revision.
    Promote,
    /// Rollback to a previous revision.
    Rollback,
}

/// Durable record of an alias promotion or rollback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromotionRecord {
    /// Unique promotion ID (e.g., `prom-abc123`).
    pub promotion_id: String,
    /// Whether this is a promote or rollback.
    pub kind: PromotionKind,
    /// Alias that was moved.
    pub alias_id: String,
    /// Logical agent ID.
    pub agent_id: String,
    /// Previous revision target (None for first promotion).
    pub previous_revision_id: Option<String>,
    /// New revision target.
    pub new_revision_id: String,
    /// Eval run that justified this promotion (if any).
    pub source_eval_run_id: Option<String>,
    /// Free-text reason.
    pub reason: Option<String>,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Actor kind.
    pub created_by_type: String,
    /// Actor ID.
    pub created_by_id: String,
    /// Origin node for provenance.
    pub origin_node_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_ref_parse_valid() {
        let r = AgentRef::parse("planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234")
            .expect("should parse");
        assert_eq!(r.agent_id, "planner.default");
        assert_eq!(
            r.revision_id,
            "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        );
        assert_eq!(
            r.to_string(),
            "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        );
    }

    #[test]
    fn test_agent_ref_parse_rejects_no_at() {
        assert!(AgentRef::parse("planner.default").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_multiple_at() {
        assert!(AgentRef::parse("planner@default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_empty_agent_id() {
        assert!(AgentRef::parse(
            "@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        )
        .is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_empty_revision() {
        assert!(AgentRef::parse("planner.default@").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_invalid_revision_format() {
        assert!(AgentRef::parse("planner.default@not-a-revision").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_wrong_hex_length() {
        assert!(AgentRef::parse("planner.default@rev_sha256:abcd").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_uppercase_hex() {
        assert!(AgentRef::parse("planner.default@rev_sha256:ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_invalid_agent_id_start() {
        assert!(AgentRef::parse(
            "-planner@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        )
        .is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_at_in_agent_id() {
        assert!(AgentRef::parse("planner@builder@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").is_none());
    }

    #[test]
    fn test_agent_alias_record_serialization() {
        let record = AgentAliasRecord {
            alias_id: "planner.default".to_string(),
            agent_id: "planner.default".to_string(),
            revision_id:
                "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
                    .to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            updated_by_type: "user".to_string(),
            updated_by_id: "admin".to_string(),
            reason: Some("initial promotion".to_string()),
        };
        let json = serde_json::to_string(&record).expect("should serialize");
        let parsed: AgentAliasRecord = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(parsed.alias_id, "planner.default");
        assert_eq!(parsed.reason, Some("initial promotion".to_string()));
    }

    #[test]
    fn test_session_agent_binding_with_explicit_ref() {
        let binding = SessionAgentBinding {
            session_id: "sess-123".to_string(),
            root_session_id: "root-123".to_string(),
            alias_id: None,
            agent_id: "planner.default".to_string(),
            revision_id: "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234".to_string(),
            runtime_lock_hash: "sha256:lock123".to_string(),
            home_node_id: "gateway-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            requested_target: "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234".to_string(),
        };
        assert!(binding.alias_id.is_none());
        assert_eq!(binding.requested_target, "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234");
    }

    #[test]
    fn test_promotion_kind_roundtrip() {
        let promote = PromotionKind::Promote;
        let json = serde_json::to_string(&promote).expect("serialize");
        assert_eq!(json, "\"promote\"");
        let rollback = PromotionKind::Rollback;
        let json = serde_json::to_string(&rollback).expect("serialize");
        assert_eq!(json, "\"rollback\"");
    }
}
